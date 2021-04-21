use anyhow::Result;
use io::{Read, Seek, SeekFrom};
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

mod dmabuf;
mod modesetting;

const RENDER_DEVICE_PATH: &'static str = "/dev/dri/card0";
const DECODE_DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "/home/alarm/FPS_test_1080p60_L4.2.h264";
const OUTPUT_MEM: MemoryType = MemoryType::Mmap;
const CAPTURE_MEM: MemoryType = MemoryType::DmaBuf;
const NUM_OUTPUT_BUFFERS: u32 = 2;
const NUM_CAPTURE_BUFFERS: u32 = 2;

/// Run a sample encoder on device `device_path`, which must be a `vicodec`
/// encoder instance. `lets_quit` will turn to true when Ctrl+C is pressed.
pub fn main() -> Result<()> {
    let h264_aud_pattern = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);
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
    let mut file =
        File::open(VIDEO_FILE_PATH).expect(&format!("Failed to load {}", VIDEO_FILE_PATH));

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

    capture_queue.set_format(capture_format.clone())?;

    // Set 640x480 RGB3 format on the OUTPUT queue.
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
    output_queue.set_format(output_format.clone())?;

    println!("Adjusted output format: {:?}", output_format);
    println!("Adjusted capture format: {:?}", capture_format);

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
    // modesetting::set_crtc(&render_device, framebuffer, mode)?;
    let fb_prime_fds: Vec<i32> = (0..NUM_CAPTURE_BUFFERS)
        .into_iter()
        .map(|i| {
            let framebuffer = modesetting::get_framebuffer(&render_device, &mode, &capture_format)
                .expect(&format!("Unable to allocate framebuffer {}", i));

            framebuffer.prime
        })
        .collect();

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
    // Encode generated frames until Ctrl+c is pressed.
    while !lets_quit.load(Ordering::SeqCst) {
        match capture_queue.try_get_free_buffer() {
            Ok(capture_buffer) => {
                // There is no information to set on Mmap capture buffers: just queue
                // them as soon as we get them.
                let dmabufs = dmabuf_handles
                    .pop_front()
                    .expect("Unable to get an available dmabuf");
                dbg!(&dmabufs);
                capture_buffer
                    .queue_with_handles(dmabufs)
                    .expect("Failed to queue capture buffer");
            }
            Err(error) => {
                eprintln!("Failed to obtain capture buffer: {:?}", error);
            }
        }

        // USERPTR output buffers, on the other hand, must be set up with
        // a user buffer and bytes_used.
        // The queue takes ownership of the buffer until the driver is done
        // with it.
        match output_queue.try_get_free_buffer() {
            Ok(output_buffer) => {
                match output_buffer {
                    GenericQBuffer::Mmap(buf) => {
                        let mut mapping = buf
                            .get_plane_mapping(0)
                            .expect("Failed to get Mmap mapping");

                        let bytes_read = file.read(&mut mapping.data)?;

                        let mat = h264_aud_pattern
                            .find(&mapping.data[1..])
                            .expect("Failed to find AUD in file");
                        let bytes_used = mat.start();
                        // dbg!(&mapping.data[..6]);
                        mapping.data[bytes_used..].fill(0);

                        let offset = bytes_read as i64 - bytes_used as i64 - 1;
                        dbg!(
                            bytes_read,
                            bytes_used,
                            file.seek(SeekFrom::Current(0))?,
                            offset
                        );
                        let new_position = file.seek(SeekFrom::Current(-offset))?;
                        dbg!(new_position);

                        buf.queue(&[bytes_used])
                            .expect("Failed to queue output buffer");

                        println!("Read {} bytes into buffer!", bytes_used);
                    }
                    GenericQBuffer::User(buf) => {
                        let mut output_buffer_data = output_frame
                            .take()
                            .expect("Output buffer not available. This is a bug.");

                        let bytes_used = file.read(&mut output_buffer_data)?;

                        buf.queue_with_handles(
                            GenericBufferHandles::from(vec![UserPtrHandle::from(
                                output_buffer_data,
                            )]),
                            &[bytes_used],
                        )
                        .expect("Failed to queue output buffer");
                    }
                    GenericQBuffer::DmaBuf(_) => todo!(),
                }
            }
            Err(error) => {
                eprintln!("Failed to obtain output buffer: {:?}", error);
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
            Err(error) => {
                eprintln!("Failed to dequeue output buffer: {:?}", error);
            }
        }

        let cap_dqbuf = capture_queue.try_dequeue();

        // If cap_dqbuf is error then it's not ready
        if cap_dqbuf.is_err() {
            eprintln!(
                "Failed to dequeue capture buffer: {:?}",
                cap_dqbuf.unwrap_err()
            );
            continue;
        }

        let cap_dqbuf = cap_dqbuf.expect("Failed to dequeue capture buffer");
        let cap_index = cap_dqbuf.data.index() as usize;
        let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;

        total_size = total_size.wrapping_add(bytes_used);
        let elapsed = start_time.elapsed();
        let fps = cpt as f64 / elapsed.as_millis() as f64 * 1000.0;
        print!(
            "\rDecoded buffer {:#5}, -> {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2}\n",
            cap_dqbuf.data.sequence(),
            cap_index,
            bytes_used,
            total_size,
            fps
        );
        io::stdout().flush().unwrap();

        // let cap_handles = cap_dqbuf.take_handles().unwrap();
        // modesetting::set_crtc(&render_device, Some(cap_handles[0]), Some(mode))
        // .expect("Unable to set first framebuffer as current crtc");
        // render_device.

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
