use aho_corasick::AhoCorasick;
use anyhow::{ensure, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
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

mod dmabuf;
mod modesetting;

const RENDER_DEVICE_PATH: &'static str = "/dev/dri/card0";
const DECODE_DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "/home/alarm/FPS_test_1080p60_L4.2.h264";
const CAPTURE_MEM: MemoryType = MemoryType::DmaBuf;
const NUM_OUTPUT_BUFFERS: usize = 2;
// const NUM_CAPTURE_BUFFERS: usize = 2;

type PooledDmaBufHandlesProvider = PooledHandlesProvider<Vec<DmaBufHandle<File>>>;

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

    let h264_aud_pattern = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);
    let poll_count_reader = Arc::new(AtomicUsize::new(0));
    let poll_count_writer = Arc::clone(&poll_count_reader);
    let start_time = std::time::Instant::now();
    let mut frame_counter = 0usize;
    // Open file for reading
    let mut file =
        File::open(VIDEO_FILE_PATH).expect(&format!("Failed to load {}", VIDEO_FILE_PATH));
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
            },
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
            let format: Format = f.set_pixelformat(b"H264").set_size(1920, 1080).apply()?;

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

    let mut total_read: usize = 0;
    let file_size = file.metadata()?.len();

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

        let bytes_read = file.read(&mut mapping.data)?;
        let mat = h264_aud_pattern
            .find(&mapping.data[1..])
            .expect("Failed to find AUD in file");
        let bytes_used = mat.start() + 1;
        total_read += bytes_used;
        // mapping.data[bytes_used..].fill(0);

        let offset = bytes_read as i64 - bytes_used as i64;
        let new_position = file.seek(SeekFrom::Current(-offset))?;
        debug_assert_eq!(total_read, new_position as usize);

        println!(
            "Filled buffer with {} bytes, new_position: {}, len: {}, read: {}",
            bytes_used, new_position, file_size, total_read
        );
        v4l2_buffer
            .queue(&[bytes_used])
            .expect("Failed to queue input frame");
    }

    decoder.drain(true).unwrap();
    decoder.stop().unwrap();
    println!();

    Ok(())
}
