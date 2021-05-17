use anyhow::Result;
use drm::control::{Event, PageFlipFlags};
use io::{BufRead, BufReader, Read};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use aho_corasick::AhoCorasick;
use qbuf::{
    get_free::{GetFreeCaptureBuffer, GetFreeOutputBuffer},
    CaptureQueueable,
};
use v4l2r::{
    device::queue::qbuf::get_indexed::GetCaptureBufferByIndex,
    ioctl::{subscribe_event, DqBufError, DqEventError, EventType, SubscribeEventFlags},
};
use v4l2r::{device::queue::qbuf::OutputQueueable, Format};
use v4l2r::{
    device::queue::*,
    memory::{DmaBufHandle, MemoryType},
};
use v4l2r::{
    device::{
        queue::generic::{GenericBufferHandles, GenericQBuffer, GenericSupportedMemoryType},
        AllocatedQueue, Device, DeviceConfig, Stream, TryDequeue,
    },
    memory::UserPtrHandle,
};

use crate::modesetting::PrimeFramebuffer;

mod dmabuf;
mod modesetting;

const RENDER_DEVICE_PATH: &'static str = "/dev/dri/card0";
const DECODE_DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "/home/alarm/FPS_test_1080p60_L4.2.h264";
const OUTPUT_MEM: MemoryType = MemoryType::Mmap;
const CAPTURE_MEM: MemoryType = MemoryType::DmaBuf;
const NUM_OUTPUT_BUFFERS: u32 = 5;
const NUM_CAPTURE_BUFFERS: u32 = 2;

lazy_static::lazy_static! {
    static ref H264_AUD_PATTERN: AhoCorasick = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);
}

/// Run a sample encoder on device `device_path`, which must be a `vicodec`
/// encoder instance. `lets_quit` will turn to true when Ctrl+C is pressed.
pub fn main() -> Result<()> {
    let decode_path = Path::new(DECODE_DEVICE_PATH);

    let lets_quit = Arc::new(AtomicBool::new(false));
    // Setup the Ctrl+c handler.
    {
        let lets_quit_handler = lets_quit.clone();
        ctrlc::set_handler(move || {
            lets_quit_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl-C handler.");
    }

    // Open file for reading
    let mut file = BufReader::with_capacity(
        100_000,
        File::open(VIDEO_FILE_PATH).expect(&format!("Failed to load {}", VIDEO_FILE_PATH)),
    );

    let device = Device::open(decode_path, DeviceConfig::new().non_blocking_dqbuf())
        .expect("Failed to open device");
    let caps = &device.capability;
    println!(
        "Opened device: {}\n\tdriver: {}\n\tbus: {}\n\tcapabilities: {}",
        caps.card, caps.driver, caps.bus_info, caps.capabilities
    );
    // if caps.driver != "vicodec" {
    //     panic!(
    //         "This device is {}, but this test is designed to work with the vicodec driver.",
    //         caps.driver
    //     );
    // }

    let device = Arc::new(device);

    // Obtain the queues, depending on whether we are using the single or multi planar API.
    let (mut output_queue, mut capture_queue, use_multi_planar) = if let Ok(output_queue) =
        Queue::get_output_queue(Arc::clone(&device))
    {
        (
            output_queue,
            Queue::get_capture_queue(Arc::clone(&device)).expect("Failed to obtain capture queue"),
            false,
        )
    } else if let Ok(output_queue) = Queue::get_output_mplane_queue(Arc::clone(&device)) {
        (
            output_queue,
            Queue::get_capture_mplane_queue(Arc::clone(&device))
                .expect("Failed to obtain capture queue"),
            true,
        )
    } else {
        panic!("Both single-planar and multi-planar queues are unusable.");
    };

    println!(
        "Multi-planar: {}",
        if use_multi_planar { "yes" } else { "no" }
    );

    println!("Output capabilities: {:?}", output_queue.get_capabilities());
    println!(
        "Capture capabilities: {:?}",
        capture_queue.get_capabilities()
    );

    println!("Output formats:");
    for fmtdesc in output_queue.format_iter() {
        println!("\t{}", fmtdesc);
    }

    println!("Capture formats:");
    for fmtdesc in capture_queue.format_iter() {
        println!("\t{}", fmtdesc);
    }

    // Make sure the CAPTURE queue will produce FWHT.
    let capture_format: Format = capture_queue
        .change_format()
        .expect("Failed to get capture format")
        .set_size(1920, 1080)
        .set_pixelformat(b"RGBP")
        .apply()
        .expect("Failed to set capture format");

    if capture_format.pixelformat != b"RGBP".into() {
        panic!("RGBP format not supported on CAPTURE queue.");
    }

    // Set 1920x1080 H264 format on the OUTPUT queue.
    let output_format: Format = output_queue
        .change_format()
        .expect("Failed to get output format")
        .set_size(1920, 1080)
        .set_pixelformat(b"H264")
        .apply()
        .expect("Failed to set output format");

    if output_format.pixelformat != b"H264".into() {
        panic!("H264 format not supported on OUTPUT queue.");
    }
    // output_queue.set_format(output_format.clone())?;

    println!("Adjusted output format: {:?}", output_format);
    println!("Adjusted capture format: {:?}", capture_format);

    subscribe_event(
        &*device,
        EventType::SourceChange,
        SubscribeEventFlags::empty(),
    )?;
    subscribe_event(&*device, EventType::Eos, SubscribeEventFlags::empty())?;

    let output_image_size = output_format.plane_fmt[0].sizeimage as usize;

    match CAPTURE_MEM {
        // MemoryType::Mmap => (),
        MemoryType::DmaBuf => (),
        m => panic!("Unsupported capture memory type {:?}", m),
    }

    // Move the queues into their "allocated" state.

    let output_mem = match OUTPUT_MEM {
        MemoryType::Mmap => GenericSupportedMemoryType::Mmap,
        MemoryType::UserPtr => GenericSupportedMemoryType::UserPtr,
        MemoryType::DmaBuf => panic!("DmaBuf is not supported yet!"),
    };

    let output_queue = output_queue
        .request_buffers_generic::<GenericBufferHandles>(output_mem, NUM_OUTPUT_BUFFERS)
        .expect("Failed to allocate output buffers");

    let capture_queue = capture_queue
        .request_buffers::<Vec<DmaBufHandle<File>>>(NUM_CAPTURE_BUFFERS)
        .expect("Failed to allocate output buffers");
    println!(
        "Using {} output and {} capture buffers.",
        output_queue.num_buffers(),
        capture_queue.num_buffers()
    );

    // If we use UserPtr OUTPUT buffers, create backing memory.
    let mut output_frame = match output_mem {
        GenericSupportedMemoryType::Mmap => None,
        GenericSupportedMemoryType::UserPtr => Some(vec![0u8; output_image_size]),
        GenericSupportedMemoryType::DmaBuf => todo!(),
    };

    let render_device = modesetting::Card::open(RENDER_DEVICE_PATH)
        .expect(&format!("Unable to open {}", RENDER_DEVICE_PATH));
    let mode =
        modesetting::get_mode(&render_device).expect("Failed to get prefered framebuffer mode");

    let framebuffers: Vec<PrimeFramebuffer> = (0..NUM_CAPTURE_BUFFERS)
        .into_iter()
        .map(|i| {
            modesetting::get_framebuffer(&render_device, &mode, &capture_format)
                .expect(&format!("Unable to allocate framebuffer {}", i))
        })
        .collect();

    let crtc = modesetting::set_crtc(&render_device, Some(framebuffers[0].handle), Some(mode))
        .expect("Unable to set first framebuffer as current crtc");

    let fb_prime_fds = framebuffers.iter().map(|fb| fb.prime).collect::<Vec<i32>>();

    let mut dmabuf_handles = dmabuf::register_dmabufs(
        decode_path,
        capture_queue.get_type(),
        &capture_format,
        &fb_prime_fds,
    )
    .expect("Could not register a DMABuf");
    // let pooled_dma_buffers = PooledDMABufHandlesProvider::new(dmabuf_handles);

    output_queue
        .stream_on()
        .expect("Failed to start output_queue");
    capture_queue
        .stream_on()
        .expect("Failed to start capture_queue");

    let mut cpt = 0usize;
    let mut total_size = 0usize;
    let start_time = Instant::now();
    let mut cap_handles = None;
    let mut pending_page_flip = false;
    let mut on_screen_capture_buffer_idx = 0;
    // Encode generated frames until Ctrl+c is pressed.
    while !lets_quit.load(Ordering::SeqCst) {
        match v4l2r::ioctl::dqevent(&*device) {
            Err(DqEventError::NotReady) => {}
            Ok(v4l2r::ioctl::Event::SrcChangeEvent(change)) => {
                dbg!(change);
                let out_format: Format = output_queue.get_format()?;
                let cap_format: Format = capture_queue.get_format()?;
                dbg!(out_format);
                dbg!(cap_format);
            }
            Err(error) => {
                return Err(error.into());
            }
        }

        let offscreen_buffer_index = match on_screen_capture_buffer_idx {
            1 => 0,
            0 => 1,
            _ => 0,
        };
        match capture_queue.try_get_buffer(offscreen_buffer_index) {
            Ok(capture_buffer) => {
                // There is no information to set on Mmap capture buffers: just queue
                // them as soon as we get them.
                if let Some(handle) = cap_handles.take() {
                    dmabuf_handles.push_back(handle);
                }
                println!("Free capture buffer index: {}", capture_buffer.index());
                let dmabufs = dmabuf_handles
                    .pop_front()
                    .expect("Unable to get an available dmabuf");
                capture_buffer
                    .queue_with_handles(dmabufs)
                    .expect("Failed to queue capture buffer");
            }
            Err(_error) => {
                // eprintln!("Failed to obtain capture buffer: {:?}", error);
            }
        }

        // USERPTR output buffers, on the other hand, must be set up with
        // a user buffer and bytes_used.
        // The queue takes ownership of the buffer until the driver is done
        // with it.
        match output_queue.try_get_free_buffer() {
            Ok(output_buffer) => match output_buffer {
                GenericQBuffer::Mmap(buf) => {
                    let mut mapping = buf
                        .get_plane_mapping(0)
                        .expect("Failed to get Mmap mapping");

                    let bytes_used = read_next_aud(&mut file, &mut mapping.data)?;

                    buf.queue(&[bytes_used])
                        .expect("Failed to queue output buffer");

                    // println!("Read {} bytes into buffer!", bytes_used);
                }
                GenericQBuffer::User(buf) => {
                    let mut output_buffer_data = output_frame
                        .take()
                        .expect("Output buffer not available. This is a bug.");

                    let bytes_used = file.read(&mut output_buffer_data)?;

                    buf.queue_with_handles(
                        GenericBufferHandles::from(vec![UserPtrHandle::from(output_buffer_data)]),
                        &[bytes_used],
                    )
                    .expect("Failed to queue output buffer");
                }
                GenericQBuffer::DmaBuf(_) => todo!(),
            },
            Err(_error) => {
                // eprintln!("Failed to obtain output buffer: {:?}", error);
            }
        }

        // Now dequeue the work that we just scheduled.
        match output_queue.try_dequeue() {
            Ok(mut out_dqbuf) => {
                // unwrap() is safe here as we just dequeued the buffer.
                match &mut out_dqbuf.take_handles().unwrap() {
                    // For Mmap buffers we can just drop the reference.
                    GenericBufferHandles::Mmap(_) => (),
                    // For UserPtr buffers, make the buffer data available again. It
                    // should have been empty since the buffer was owned by the queue.
                    GenericBufferHandles::User(u) => {
                        assert_eq!(output_frame.replace(u.remove(0).0), None);
                    }
                    GenericBufferHandles::DmaBuf(_) => todo!(),
                }
            }
            Err(_error) => {
                // eprintln!("Failed to dequeue output buffer: {:?}", error);
            }
        }

        let cap_dqbuf = capture_queue.try_dequeue();

        // If cap_dqbuf is error then it's not ready
        if cap_dqbuf.is_err() {
            let error = cap_dqbuf.unwrap_err();
            match error {
                DqBufError::NotReady => continue,
                _ => {
                    eprintln!("Failed to dequeue capture buffer: {:?}", error);
                    break;
                }
            }
        }

        let mut cap_dqbuf = cap_dqbuf.expect("Failed to dequeue capture buffer");
        let cap_index = cap_dqbuf.data.index() as usize;
        let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;

        total_size = total_size.wrapping_add(bytes_used);
        let elapsed = start_time.elapsed();
        let fps = cpt as f64 / elapsed.as_millis() as f64 * 1000.0;
        println!(
            "Decoded buffer {:#5}, -> {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2}",
            cap_dqbuf.data.sequence(),
            cap_index,
            bytes_used,
            total_size,
            fps
        );
        io::stdout().flush().unwrap();

        cap_handles = Some(cap_dqbuf.take_handles().unwrap());

        // Change front buffer to newly decoded buffer
        use drm::control::Device;

        // This makes sure we are not faster than screen refresh rate, basically VSYNC
        if pending_page_flip {
            'get_events: loop {
                let events = render_device.receive_events()?;
                for event in events {
                    if matches!(event, Event::PageFlip(_)) {
                        break 'get_events;
                    }
                }
            }
        }
        render_device.page_flip(
            crtc,
            framebuffers[cap_index].handle,
            &[PageFlipFlags::PageFlipEvent],
            None,
        )?;
        pending_page_flip = true;
        on_screen_capture_buffer_idx = cap_index;

        cpt = cpt.wrapping_add(1);
    }

    capture_queue
        .stream_off()
        .expect("Failed to stop output_queue");
    output_queue
        .stream_off()
        .expect("Failed to stop output_queue");

    Ok(())
}

// Read more data from the file into dst until we run into the pattern [0,0,0,1]
fn read_next_aud(file: &mut BufReader<File>, dst: &mut [u8]) -> Result<usize> {
    let mut bytes_used = 0;

    loop {
        let buffer = file.fill_buf()?;

        // Check if buffer is empty right after filling it. If it is it means we've hit the end of the file.
        if buffer.is_empty() {
            return Err(anyhow::anyhow!("End of file"));
        }

        // Search for the
        match H264_AUD_PATTERN.find(&buffer[1..]) {
            Some(mat) => {
                let dst_end = bytes_used + mat.start() + 1;
                // println!("Read from {:x} to {:x}", bytes_used, dst_end);
                file.read_exact(&mut dst[bytes_used..dst_end])?;
                bytes_used += mat.start() + 1;
                break;
            }
            None => {
                let buffer_len = buffer.len();
                let dst_end = bytes_used + buffer_len;
                file.read_exact(&mut dst[bytes_used..dst_end])?;
                bytes_used += buffer_len;
            }
        }
    }

    let buffer = file.fill_buf()?;
    // The buffer we have filled should start with the aud pattern
    debug_assert_eq!(&dst[..4], &[0, 0, 0, 1]);

    // The buffer we will create next should start with the aud pattern
    if !buffer.is_empty() {
        debug_assert_eq!(&buffer[..4], &[0, 0, 0, 1]);
    }

    Ok(bytes_used)
}
