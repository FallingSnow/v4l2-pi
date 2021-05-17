use aho_corasick::AhoCorasick;
use lazy_static::lazy_static;
use nix::sys::time::{TimeVal, TimeValLike};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{collections::VecDeque, path::Path};
use std::{
    fs::File,
    io::{BufRead, BufReader},
};
use std::{io::Read, os::unix::prelude::AsRawFd};
use v4l2r::{
    device::queue::qbuf::get_free::GetFreeOutputBuffer,
    memory::{DmaBufHandle, MemoryType},
    Format,
};

mod dmabuf;
mod modesetting;

const RENDER_DEVICE_PATH: &'static str = "/dev/dri/card0";
const DECODE_DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "/home/alarm/FPS_test_1080p60_L4.2.h264";
const OUTPUT_MEM: MemoryType = MemoryType::Mmap;
// const CAPTURE_MEM: MemoryType = MemoryType::DmaBuf;
const NUM_OUTPUT_BUFFERS: u32 = 3;
const NUM_CAPTURE_BUFFERS: u32 = 2;

lazy_static! {
    static ref H264_AUD_PATTERN: AhoCorasick = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);
}

/// Run a sample encoder on device `device_path`, which must be a `vicodec`
/// encoder instance. `lets_quit` will turn to true when Ctrl+C is pressed.
#[async_std::main]
async fn main() -> std::io::Result<()> {
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

    // ********
    // Open decoding device
    // ********
    let decode_device = {
        use v4l2r::device::{Device, DeviceConfig};
        let device = Device::open(decode_path, DeviceConfig::new()).expect("Failed to open device");
        let caps = &device.capability;
        println!(
            "Opened device: {}\n\tdriver: {}\n\tbus: {}\n\tcapabilities: {}",
            caps.card, caps.driver, caps.bus_info, caps.capabilities
        );

        Arc::new(device)
    };

    // ********
    // Create capture and output queue
    // ********
    let (mut output_queue, mut capture_queue, _use_multi_planar) = {
        use v4l2r::device::queue::Queue;

        // Obtain the queues, depending on whether we are using the single or multi planar API.
        let (output_queue, capture_queue, use_multi_planar) = if let Ok(output_queue) =
            Queue::get_output_queue(Arc::clone(&decode_device))
        {
            (
                output_queue,
                Queue::get_capture_queue(Arc::clone(&decode_device))
                    .expect("Failed to obtain capture queue"),
                false,
            )
        } else if let Ok(output_queue) = Queue::get_output_mplane_queue(Arc::clone(&decode_device))
        {
            (
                output_queue,
                Queue::get_capture_mplane_queue(Arc::clone(&decode_device))
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

        (output_queue, capture_queue, use_multi_planar)
    };

    // ********
    // Set output and capture formats
    // ********
    let capture_format = {
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

        capture_format
    };

    let output_mem = {
        use v4l2r::device::queue::generic::GenericSupportedMemoryType;

        match OUTPUT_MEM {
            MemoryType::Mmap => GenericSupportedMemoryType::Mmap,
            MemoryType::UserPtr => panic!("UserPtr is not supported by Raspberry Pi!"),
            MemoryType::DmaBuf => panic!("DmaBuf is not supported yet!"),
        }
    };

    // ********
    // Allocate buffers on the decoding device for the capture and output queues
    // ********
    let (output_queue, capture_queue) = {
        use v4l2r::device::queue::generic::GenericBufferHandles;
        use v4l2r::device::AllocatedQueue;

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

        (output_queue, capture_queue)
    };

    // ********
    // Initialize rendering device (DRM)
    // ********
    let (render_device, framebuffers, crtc, mut dmabuf_handles) = {
        use crate::modesetting::PrimeFramebuffer;

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

        // Set crtc mode and use framebuffer[0] for the time being to fill the screen
        let crtc = modesetting::set_crtc(&render_device, Some(framebuffers[0].handle), Some(mode))
            .expect("Unable to set first framebuffer as current crtc");

        let fb_prime_fds = framebuffers
            .iter()
            .map(|fb| {
                println!("{:?}, prime_fd: {}", fb.handle, fb.prime);
                fb.prime
            })
            .collect::<Vec<i32>>();

        let dmabuf_handles = dmabuf::register_dmabufs(
            decode_path,
            capture_queue.get_type(),
            &capture_format,
            &fb_prime_fds,
        )
        .expect("Could not register a DMABuf");

        (render_device, framebuffers, crtc, dmabuf_handles)
    };

    // ********
    // Preload decoder with dma capture buffers
    // ********
    while let Ok(buf) =
        v4l2r::device::queue::qbuf::get_free::GetFreeCaptureBuffer::try_get_free_buffer(
            &capture_queue,
        )
    {
        use v4l2r::device::queue::qbuf::CaptureQueueable;
        dmabuf_handles.reverse();
        let dmabuf = dmabuf_handles.pop().expect("Failed to get dmabuf_handle");
        buf.queue_with_handles(dmabuf)
            .expect("Failed to queue capture buffer");
    }

    // ********
    // Preload decoder with h264 data
    // ********
    while let Ok(buf) = output_queue.try_get_free_buffer() {
        use v4l2r::device::queue::generic::GenericQBuffer;

        match buf {
            GenericQBuffer::Mmap(buf) => {
                let mut mapping = buf
                    .get_plane_mapping(0)
                    .expect("Failed to get Mmap mapping");

                let bytes_used = read_next_aud(&mut file, &mut mapping.data)?;

                buf.queue(&[bytes_used])
                    .expect("Failed to queue output buffer");
            }
            _ => unimplemented!(),
        }
    }

    // ********
    // Create awaitable aync file descriptors
    // ********
    // let poller = {
    //     use nix::sys::epoll::epoll_create1;
    //     use nix::sys::epoll::epoll_ctl;
    //     use nix::sys::epoll::EpollCreateFlags;
    //     use nix::sys::epoll::EpollEvent;
    //     use nix::sys::epoll::EpollFlags;
    //     use nix::sys::epoll::EpollOp::EpollCtlAdd;

    //     let mut decode_event = EpollEvent::new(
    //         EpollFlags::EPOLLET | EpollFlags::EPOLLIN | EpollFlags::EPOLLOUT | EpollFlags::EPOLLPRI,
    //         decode_device.as_raw_fd().try_into().unwrap(),
    //     );
    //     let mut render_event = EpollEvent::new(
    //         EpollFlags::EPOLLET | EpollFlags::EPOLLPRI,
    //         render_device.as_raw_fd().try_into().unwrap(),
    //     );
    //     let poller =
    //         epoll_create1(EpollCreateFlags::EPOLL_CLOEXEC).expect("Failed to create event poller");
    //     epoll_ctl(
    //         poller,
    //         EpollCtlAdd,
    //         decode_device.as_raw_fd(),
    //         Some(&mut decode_event),
    //     )
    //     .expect("Unable to add pollable decode device");
    //     epoll_ctl(
    //         poller,
    //         EpollCtlAdd,
    //         render_device.as_raw_fd(),
    //         Some(&mut render_event),
    //     )
    //     .expect("Unable to add pollable render device");

    //     println!(
    //         "Poller created and listening for events on {} and {}",
    //         decode_device.as_raw_fd(),
    //         render_device.as_raw_fd()
    //     );

    //     poller
    // };

    // {
    //     let start_time = std::time::Instant::now();
    //     let mut num_captured_frames = 0usize;
    //     let mut total_size = 0usize;
    //     let mut current_buffer: Option<
    //         v4l2r::device::queue::dqbuf::DqBuffer<
    //             v4l2r::device::queue::direction::Capture,
    //             Vec<DmaBufHandle<File>>,
    //         >,
    //     > = None;

    //     // ********
    //     // Event Loop
    //     // ********
    //     while !lets_quit.load(Ordering::SeqCst) {
    //         use nix::sys::epoll::epoll_wait;
    //         use nix::sys::epoll::EpollEvent;
    //         use nix::sys::epoll::EpollFlags;
    //         use v4l2r::device::TryDequeue;

    //         enum Event {
    //             CaptureReady,
    //             OutputReady,
    //             DecodeError(nix::errno::Errno),
    //             RenderReadable,
    //         }

    //         let mut event_queue = [EpollEvent::empty(); 2];

    //         println!("Polling for events!");
    //         let num_events = epoll_wait(poller, &mut event_queue, 1000).expect("Unable to poll for events");

    //         // ********
    //         // Handle events
    //         // ********
    //         let mut filtered_events = Vec::with_capacity(6);
    //         for event in event_queue {
    //             // If is decoding device event
    //             dbg!(event);
    //             if event.data() == decode_device.as_raw_fd() as u64 {
    //                 if event.events().contains(EpollFlags::EPOLLIN) {
    //                     // Capture
    //                     filtered_events.push(Event::CaptureReady);
    //                 }
    //                 if event.events().contains(EpollFlags::EPOLLOUT) {
    //                     filtered_events.push(Event::OutputReady);
    //                 }
    //             }
    //             if event.events().contains(EpollFlags::EPOLLERR) {
    //                 if event.data() == decode_device.as_raw_fd() as u64 {
    //                     filtered_events.push(Event::DecodeError(nix::errno::Errno::last()));
    //                 }
    //             }
    //         }
    //         println!("Got {} filtered events", filtered_events.len());

    //         for event in filtered_events {
    //             match event {
    //                 Event::DecodeError(err) => {
    //                     println!("Decoder error = {}!", err);
    //                 }
    //                 Event::RenderReadable => {
    //                     println!("Render readable");
    //                     let drm_events = drm::control::Device::receive_events(&render_device)
    //                         .expect("Unable to get render events");

    //                     for event in drm_events {
    //                         use drm::control::Event;
    //                         use v4l2r::device::queue::qbuf::get_indexed::GetCaptureBufferByIndex;
    //                         use v4l2r::device::queue::qbuf::CaptureQueueable;

    //                         if let Event::PageFlip(_flip) = event {
    //                             if let Some(mut buffer) = current_buffer.take() {
    //                                 // print!(
    //                                 //     "\rDecoded buffer {:#5}, {:#2} -> {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2}",
    //                                 //     cap_dqbuf.data.sequence(),
    //                                 //     cap_index,
    //                                 //     index,
    //                                 //     bytes_used,
    //                                 //     total_size,
    //                                 //     fps
    //                                 // );
    //                                 // std::io::Write::flush(&mut std::io::stdout())?;

    //                                 // println!("Buffer #{} removed from display", index);
    //                                 let index = buffer.data.index() as usize;
    //                                 let handles = buffer
    //                                     .take_handles()
    //                                     .expect("Dequeued buffer had no planes handles");
    //                                 drop(buffer);

    //                                 // println!("Queuing buffer #{}", index);
    //                                 let q_buf = capture_queue
    //                                     .try_get_buffer(index)
    //                                     .expect("Unable to aquire freed buffer");
    //                                 q_buf
    //                                     .queue_with_handles(handles)
    //                                     .expect("Failed to queue capture buffer");
    //                             }
    //                             // println!("New current buffer is {}", cap_dqbuf.data.index());
    //                         }
    //                     }
    //                 }
    //                 Event::CaptureReady => {
    //                     println!("Decode readable");

    //                     use drm::control::PageFlipFlags::PageFlipEvent;

    //                     let cap_dqbuf = capture_queue
    //                         .try_dequeue()
    //                         .expect("Capture ready to dequeue but failed");
    //                     let cap_index = cap_dqbuf.data.index();
    //                     let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;
    //                     total_size = total_size.wrapping_add(bytes_used);
    //                     let elapsed = start_time.elapsed();
    //                     let fps = num_captured_frames as f64 / elapsed.as_millis() as f64 * 1000.0;

    //                     println!("Buffer #{} ready to display", cap_index);

    //                     if current_buffer.is_some() {
    //                         debug_assert!(
    //                             cap_index > current_buffer.as_ref().unwrap().data.index()
    //                                 || cap_index == 0
    //                         );
    //                     }

    //                     drm::control::Device::page_flip(
    //                         &render_device,
    //                         crtc,
    //                         framebuffers[cap_index as usize].handle,
    //                         &[PageFlipEvent],
    //                         None,
    //                     )
    //                     .expect("Unable to swap to new framebuffer");

    //                     current_buffer = Some(cap_dqbuf);

    //                     num_captured_frames = num_captured_frames.wrapping_add(1);
    //                 }
    //                 Event::OutputReady => {
    //                     println!("Decode writable");

    //                     use v4l2r::device::queue::generic::GenericQBuffer;

    //                     output_queue
    //                         .try_dequeue()
    //                         .expect("Cannot dequeue an output buffer");

    //                     let buf = output_queue
    //                         .try_get_free_buffer()
    //                         .expect("Output is ready but there is no free output buffer");

    //                     match buf {
    //                         GenericQBuffer::Mmap(buf) => {
    //                             let mut mapping = buf
    //                                 .get_plane_mapping(0)
    //                                 .expect("Failed to get Mmap mapping");

    //                             let bytes_used = read_next_aud(&mut file, &mut mapping.data)?;

    //                             buf.queue(&[bytes_used])
    //                                 .expect("Failed to queue output buffer");
    //                         }
    //                         _ => unimplemented!(),
    //                     }
    //                 }
    //                 _ => unimplemented!(),
    //             }
    //         }
    //     }
    // }

    // ********
    // Event Loop
    // ********
    {
        use drm::control::PageFlipFlags::PageFlipEvent;
        use v4l2r::device::poller::{DeviceEvent, PollEvent, Poller};
        use v4l2r::device::queue::generic::GenericQBuffer;

        let mut poller =
            Poller::new(decode_device.clone()).expect("Unable to create device listener");
        poller
            .enable_event(DeviceEvent::CaptureReady)
            .expect("Could not start listening for capture ready events");
        poller
            .enable_event(DeviceEvent::OutputReady)
            .expect("Could not start listening for output ready events");
        poller
            .enable_event(DeviceEvent::V4L2Event)
            .expect("Could not start listening for v4l2 events");

        // ********
        // Start both output and capture queue processing
        // Raspberry PI requires both to be on to signal a source change event
        // ********
        {
            use v4l2r::device::Stream;

            output_queue
                .stream_on()
                .expect("Failed to start output_queue");
            capture_queue
                .stream_on()
                .expect("Failed to start capture_queue");
        }

        let timeout = Some(std::time::Duration::new(0, 0));
        let start_time = std::time::Instant::now();
        let mut num_captured_frames = 0usize;
        let mut total_size = 0usize;
        let mut framebuffer_queue: VecDeque<
            v4l2r::device::queue::dqbuf::DqBuffer<
                v4l2r::device::queue::direction::Capture,
                Vec<DmaBufHandle<File>>,
            >,
        > = VecDeque::new();
        let mut flip_pending = false;

        while !lets_quit.load(Ordering::SeqCst) {
            let events = poller.poll(timeout).expect("Unable to query events");
            let drm_events = drm::control::Device::receive_events(&render_device);

            // Ignore
            let drm_events = match drm_events {
                Err(err) => match err {
                    drm::SystemError::Unknown {
                        errno: nix::errno::Errno::EAGAIN,
                    } => None,
                    err => Err(err).expect("Unable to get render events"),
                },
                Ok(events) => Some(events),
            };

            if let Some(drm_events) = drm_events {
                for event in drm_events {
                    use drm::control::Event;
                    if let Event::PageFlip(_flip) = event {
                        let buffer = framebuffer_queue.pop_front();
                        println!("Page Flipped:: {} {:?}", _flip.frame, _flip.duration);
                        // Check if another framebuffer is already ready to go
                        // If it is, queue it for a page_flip
                        if !framebuffer_queue.is_empty() {
                            let next_index = framebuffer_queue.front().unwrap().data.index();
                            flip_pending = try_flip_page(
                                &render_device,
                                crtc,
                                framebuffers[next_index as usize].handle,
                                &[PageFlipEvent],
                                None,
                            )?;

                            assert_eq!(flip_pending, true);
                        } else {
                            flip_pending = false;
                        }

                        if let Some(mut buffer) = buffer {
                            // println!("Buffer #{} removed from display", index);
                            let index = buffer.data.index() as usize;
                            let handles = buffer
                                .take_handles()
                                .expect("Dequeued buffer had no planes handles");
                            drop(buffer);

                            // println!("Queuing buffer #{}", index);
                            let q_buf = v4l2r::device::queue::qbuf::get_indexed::GetCaptureBufferByIndex::try_get_buffer(&capture_queue, index)
                            .expect("Unable to aquire freed buffer");

                            v4l2r::device::queue::qbuf::CaptureQueueable::queue_with_handles(
                                q_buf, handles,
                            )
                            .expect("Failed to queue capture buffer");
                        }
                        // println!(
                        //     "Displaying buffer #{}",
                        //     framebuffer_queue.front().unwrap().data.index()
                        // );
                    }
                }
            }

            for event in events {
                match event {
                    PollEvent::Device(device_event) => match device_event {
                        // Decoded H264 is available
                        DeviceEvent::CaptureReady => {
                            use v4l2r::device::TryDequeue;

                            let cap_dqbuf = capture_queue
                                .try_dequeue()
                                .expect("Capture ready to dequeue but failed");
                            let cap_index = cap_dqbuf.data.index();
                            let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;
                            total_size = total_size.wrapping_add(bytes_used);
                            let elapsed = start_time.elapsed();
                            let fps =
                                num_captured_frames as f64 / elapsed.as_millis() as f64 * 1000.0;

                            // print!(
                            //         "\rDecoded buffer seq: {:#5}, idx: {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2} buf_cache: {:#2}",
                            //         cap_dqbuf.data.sequence(),
                            //         cap_index,
                            //         bytes_used,
                            //         total_size,
                            //         fps,
                            //         framebuffer_queue.len()
                            //     );

                            println!("Buffer #{} ready to display", cap_index);
                            flip_pending = try_flip_page(
                                &render_device,
                                crtc,
                                framebuffers[cap_index as usize].handle,
                                &[PageFlipEvent],
                                None,
                            )?;
                            assert_eq!(flip_pending, true);

                            // framebuffer_queue.push_back(cap_dqbuf);
                            // if !flip_pending {
                            //     if let Some(buffer) = framebuffer_queue.front() {
                            //         flip_pending = try_flip_page(
                            //             &render_device,
                            //             crtc,
                            //             framebuffers[buffer.data.index() as usize].handle,
                            //             &[PageFlipEvent],
                            //             None,
                            //         )?;
                            //         assert_eq!(flip_pending, true);
                            //     }
                            // }

                            num_captured_frames = num_captured_frames.wrapping_add(1);

                            use std::{thread, time};
                            let ten_millis = time::Duration::from_millis(500);
                            thread::sleep(ten_millis);
                        }

                        // The decoder wants more encoded data to decode
                        DeviceEvent::OutputReady => {
                            use v4l2r::device::TryDequeue;
                            output_queue
                                .try_dequeue()
                                .expect("Cannot dequeue an output buffer");

                            let buf = output_queue
                                .try_get_free_buffer()
                                .expect("Output is ready but there is no free output buffer");

                            match buf {
                                GenericQBuffer::Mmap(buf) => {
                                    let mut mapping = buf
                                        .get_plane_mapping(0)
                                        .expect("Failed to get Mmap mapping");

                                    let bytes_used = read_next_aud(&mut file, &mut mapping.data)?;

                                    let start = std::time::SystemTime::now();
                                    let since_the_epoch = start
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .expect("Time went backwards");

                                    let buf = buf.set_timestamp(TimeVal::milliseconds(
                                        std::convert::TryInto::try_into(
                                            since_the_epoch.as_millis(),
                                        )
                                        .unwrap(),
                                    ));

                                    buf.queue(&[bytes_used])
                                        .expect("Failed to queue output buffer");
                                }
                                _ => unimplemented!(),
                            }
                        }

                        DeviceEvent::V4L2Event => {
                            dbg!("V4l2 Event Ready.");
                            match v4l2r::ioctl::dqevent(&decode_device.as_raw_fd()) {
                                Err(v4l2r::ioctl::DqEventError::NotReady) => {}
                                Ok(v4l2r::ioctl::Event::SrcChangeEvent(change)) => {
                                    dbg!(change);
                                    let out_format: Format = output_queue
                                        .get_format()
                                        .expect("Unable to get output queue format");
                                    let cap_format: Format = capture_queue
                                        .get_format()
                                        .expect("Unable to get capture queue format");
                                    dbg!(out_format);
                                    dbg!(cap_format);
                                }
                                Err(error) => {
                                    return Err(error).expect("Error dequeueing a V4L2 event");
                                }
                            }
                        }
                    },
                    _ => unimplemented!(),
                }
            }
        }
    }

    // ********
    // Stop the decoder
    // ********
    {
        use v4l2r::device::Stream;
        capture_queue
            .stream_off()
            .expect("Failed to stop output_queue");
        output_queue
            .stream_off()
            .expect("Failed to stop output_queue");
    }

    Ok(())
}

// Read more data from the file into dst until we run into the pattern [0,0,0,1]
fn read_next_aud(file: &mut BufReader<File>, dst: &mut [u8]) -> std::io::Result<usize> {
    let mut bytes_used = 0;

    loop {
        let buffer = file.fill_buf()?;

        // Check if buffer is empty right after filling it. If it is it means we've hit the end of the file.
        if buffer.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "End of file",
            ));
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
fn read_next(file: &mut BufReader<File>, dst: &mut [u8]) -> std::io::Result<usize> {
    Ok(file.read(dst)?)
}

pub fn wait_for_vblank(fd: &dyn AsRawFd, sequence: u32) -> std::io::Result<()> {
    unsafe {
        let mut data = drm_ffi::drm_wait_vblank {
            request: drm_ffi::drm_wait_vblank_request {
                type_: drm_ffi::drm_vblank_seq_type::_DRM_VBLANK_RELATIVE,
                sequence,
                signal: 0,
            },
        };
        drm_ffi::ioctl::wait_vblank(fd.as_raw_fd(), &mut data).expect("Unable to wait for vblank");
    }

    Ok(())
}

pub fn try_flip_page<T: drm::control::Device>(
    device: &T,
    crtc: drm::control::crtc::Handle,
    fb: drm::control::framebuffer::Handle,
    flags: &[drm::control::PageFlipFlags],
    target: Option<drm::control::PageFlipTarget>,
) -> std::io::Result<bool> {
    println!("Flipping to {:?}", fb);
    let flip_result = drm::control::Device::page_flip(device, crtc, fb, flags, target);

    match flip_result {
        Err(drm::SystemError::Unknown {
            errno: nix::errno::Errno::EBUSY,
        }) => Ok(false),
        Err(e) => Err(e).expect("Unable to swap to new framebuffer"),
        Ok(()) => Ok(true),
    }
}
