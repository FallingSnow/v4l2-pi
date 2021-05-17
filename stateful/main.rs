use aho_corasick::AhoCorasick;
use anyhow::{ensure, Result};
use std::{io::{Read}, os::unix::prelude::AsRawFd};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::{
    fs::File,
    io::{BufRead, BufReader},
};
use v4l2r::{
    decoder::{
        stateful::{Decoder, GetBufferError},
        DecoderEvent, FormatChangedReply,
    },
    device::{
        poller::PollError,
        queue::handles_provider::{PooledHandles, PooledHandlesProvider},
        queue::{direction::Capture, dqbuf::DqBuffer, FormatBuilder},
    },
    memory::{DmaBufHandle, DmaBufferHandles, MemoryType, MmapHandle},
    Format, QueueType, Rect,
};
use lazy_static::lazy_static;

mod dmabuf;
mod modesetting;

type PooledDmaBufHandlesProvider = PooledHandlesProvider<Vec<DmaBufHandle<File>>>;

const RENDER_DEVICE_PATH: &'static str = "/dev/dri/card0";
const DECODE_DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "/home/alarm/FPS_test_1080p60_L4.2.h264";
const CAPTURE_MEM: MemoryType = MemoryType::DmaBuf;
const NUM_OUTPUT_BUFFERS: usize = 2;

lazy_static! {
    static ref H264_AUD_PATTERN: AhoCorasick = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);
}

/// Run a sample encoder on device `device_path`, which must be a `vicodec`
/// encoder instance. `lets_quit` will turn to true when Ctrl+C is pressed.
pub fn main() -> Result<()> {
    let lets_quit = Arc::new(AtomicBool::new(false));
    // Setup the Ctrl+c handler.
    {
        let lets_quit_handler = lets_quit.clone();
        ctrlc::set_handler(move || {
            lets_quit_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl-C handler.");
    }

    let poll_count_reader = Arc::new(AtomicUsize::new(0));
    let poll_count_writer = Arc::clone(&poll_count_reader);
    let start_time = std::time::Instant::now();
    let mut frame_counter = 0usize;
    // Open file for reading
    let mut file = BufReader::new(
        File::open(VIDEO_FILE_PATH).expect(&format!("Failed to load {}", VIDEO_FILE_PATH)),
    );
    let render_device = modesetting::Card::open(RENDER_DEVICE_PATH)
        .expect(&format!("Unable to open {}", RENDER_DEVICE_PATH));

    let mode =
        modesetting::get_mode(&render_device).expect("Failed to get prefered framebuffer mode");

    let mut output_ready_cb =
        move |mut cap_dqbuf: DqBuffer<Capture, PooledHandles<DmaBufferHandles<File>>>| {
            let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;
            // Ignore zero-sized buffers.
            if bytes_used == 0 {
                return;
            }

            let elapsed = start_time.elapsed();
            frame_counter += 1;
            let fps = frame_counter as f32 / elapsed.as_millis() as f32 * 1000.0;
            let ppf = poll_count_reader.load(Ordering::SeqCst) as f32 / frame_counter as f32;
            print!(
                "\rDecoded buffer {:#5}, index: {:#2}), bytes used:{:#6} fps: {:#5.2} ppf: {:#4.2}",
                cap_dqbuf.data.sequence(),
                cap_dqbuf.data.index(),
                bytes_used,
                fps,
                ppf,
            );
            // std::io::stdout().flush().unwrap();

            let pooled_handles = cap_dqbuf.take_handles().unwrap();
            let _handles = pooled_handles.handles();
        };

    let decoder_event_cb =
        move |event: DecoderEvent<PooledHandlesProvider<Vec<DmaBufHandle<File>>>>| match event {
            DecoderEvent::FrameDecoded(dqbuf) => output_ready_cb(dqbuf),
            DecoderEvent::EndOfStream => {
                println!("End of stream!");
                ()
            }
        };

    let set_capture_format_cb =
        move |f: FormatBuilder,
              visible_rect: Rect,
              min_num_buffers: usize|
              -> anyhow::Result<FormatChangedReply<PooledDmaBufHandlesProvider>> {
            let format = f.set_pixelformat(b"RGBP").apply()?;

            println!(
                "New CAPTURE format: {:?} (visible rect: {})",
                format, visible_rect
            );

            let fb_prime_fds: Vec<i32> = (0..min_num_buffers)
                .into_iter()
                .map(|i| {
                    let framebuffer = modesetting::get_framebuffer(&render_device, &mode, &format)
                        .expect(&format!("Unable to allocate framebuffer {}", i));
                    if i == 0 {
                        modesetting::set_crtc(&render_device, Some(framebuffer.handle), Some(mode))
                            .expect("Unable to set first framebuffer as current crtc");
                    }

                    framebuffer.prime
                })
                .collect();

            let dmabuf_fds: Vec<Vec<_>> = dmabuf::register_dmabufs(
                &Path::new(&DECODE_DEVICE_PATH),
                QueueType::VideoCaptureMplane,
                &format,
                &fb_prime_fds,
            )
            .unwrap();

            Ok(FormatChangedReply {
                provider: PooledHandlesProvider::new(dmabuf_fds),
                // TODO: can't the provider report the memory type that it is
                // actually serving itself?
                mem_type: CAPTURE_MEM,
                num_buffers: min_num_buffers,
            })
        };

    let input_done_cb = |_input_buffer| {
        // println!("Input done");
        ()
    };

    let mut decoder = Decoder::open(&Path::new(&DECODE_DEVICE_PATH))
        .expect("Failed to open device")
        .set_output_format(|f| {
            let format: Format = f.set_pixelformat(b"H264").set_size(640, 480).apply()?;

            ensure!(
                format.pixelformat == b"H264".into(),
                "H264 format not supported"
            );

            println!("Temporary output format: {:?}", format);

            Ok(())
        })
        .expect("Failed to set output format")
        .allocate_output_buffers::<Vec<MmapHandle>>(NUM_OUTPUT_BUFFERS)
        .expect("Failed to allocate output buffers")
        .set_poll_counter(poll_count_writer)
        .start(input_done_cb, decoder_event_cb, set_capture_format_cb)
        .expect("Failed to start decoder");

    println!("Allocated {} buffers", decoder.num_output_buffers());

    // Raspberry Pi requires that the capture queue must also be on to fire the resolution change event
    v4l2r::ioctl::streamon(&File::open(DECODE_DEVICE_PATH)?.as_raw_fd(), v4l2r::QueueType::VideoCaptureMplane)?;

    loop {
        // Ctrl-c ?
        if lets_quit.load(Ordering::SeqCst) {
            break;
        }

        let v4l2_buffer = match decoder.get_buffer() {
            Ok(buffer) => buffer,
            // If we got interrupted while waiting for a buffer, just exit normally.
            Err(GetBufferError::PollError(PollError::EPollWait(e)))
                if e.kind() == std::io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(e) => panic!("{}", e),
        };

        let mut mapping = v4l2_buffer
            .get_plane_mapping(0)
            .expect("Failed to get Mmap mapping");

        let bytes_used = read_next_aud(&mut file, &mut mapping.data)?;

        println!("Filled buffer with {} bytes", bytes_used);
        v4l2_buffer
            .queue(&[bytes_used])
            .expect("Failed to queue input frame");
    }

    decoder.drain(true).unwrap();
    decoder.stop().unwrap();
    println!();

    Ok(())
}

fn read_next_aud(file: &mut BufReader<File>, dst: &mut [u8]) -> Result<usize> {
    let mut bytes_used = 0;

    loop {
        let buffer = file.fill_buf()?;
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
    dbg!(&bytes_used);

    Ok(bytes_used)
}
