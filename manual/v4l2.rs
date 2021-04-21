// use drm::control::{crtc, framebuffer};
use anyhow::{ensure, Result};
use std::io::{Write, Read};
use std::os::unix::io::RawFd;
use std::{fs::File, path::Path};
use v4l2r::{
    decoder::{
        stateful::{Decoder as V4l2Decoder, GetBufferError, ReadyToDecode},
        DecoderEvent, FormatChangedReply,
    },
    device::queue::{
        handles_provider::{PooledHandles, PooledHandlesProvider},
        FormatBuilder,
    },
    device::{
        poller::PollError,
        queue::{direction::Capture, dqbuf::DQBuffer},
    },
    memory::{
        DMABufHandle, DMABufSource, DMABufferHandles, MMAPHandle, MemoryType,
        PrimitiveBufferHandles,
    },
    Format, QueueType,
};

use crate::dmabuf;
use crate::mode_setting::{self, Card};

const NUM_INPUT_BUFFERS: usize = 2;
const NUM_OUTPUT_BUFFERS: usize = 2;

type PooledMMAPBufHandles = PooledHandles<Vec<MMAPHandle>>;
type PooledDMABufHandlesProvider = PooledHandlesProvider<Vec<DMABufHandle<File>>>;

pub fn init_decode(
) -> Result<V4l2Decoder<ReadyToDecode<PooledMMAPBufHandles>>> {
    let mut output_buffer_size = 0usize;

    let decoder = V4l2Decoder::open(&Path::new("/dev/video10"))?
        .set_output_format(|f: FormatBuilder| {
            let format: Format = f.set_pixelformat(b"H264").set_size(1920, 1080).apply()?;

            ensure!(
                format.pixelformat == b"H264".into(),
                "H264 format not supported"
            );

            println!("Temporary output format: {:?}", format);

            output_buffer_size = format.plane_fmt[0].sizeimage as usize;

            Ok(())
        })?
        .allocate_output_buffers::<PooledMMAPBufHandles>(NUM_OUTPUT_BUFFERS)?;

    Ok(decoder)
}

pub fn decode<OP>(decoder: V4l2Decoder<ReadyToDecode<OP>>, mut file: File) -> Result<()>
where OP: PrimitiveBufferHandles + Default,
OP::HandleType: v4l2r::memory::Mappable,
<<OP as PrimitiveBufferHandles>::HandleType as v4l2r::memory::PlaneHandle>::Memory: v4l2r::memory::SelfBacked {
    let mut decoder = decoder.start(|_| (), decoder_event_cb, set_capture_format_cb)?;

    let v4l2_buffer = match decoder.get_buffer() {
        Ok(buffer) => buffer,
        // If we got interrupted while waiting for a buffer, just exit normally.
        Err(GetBufferError::PollError(PollError::EPollWait(e)))
            if e.kind() == std::io::ErrorKind::Interrupted =>
        {
            panic!("{}", e)
        }
        Err(e) => panic!("{}", e),
    };

    let plane_buffer = v4l2_buffer.get_plane_mapping(0).unwrap();

    file.read_exact(plane_buffer.data)?;

    v4l2_buffer.queue(&[plane_buffer.data.len()])?;

    // v4l2_buffer
    //     .queue_with_handles(vec![UserPtrHandle::from(frame)], &[bytes_used])
    //     .expect("Failed to queue input frame");
    dbg!(v4l2_buffer.index());

    decoder.drain(true).unwrap();
    decoder.stop().unwrap();

    Ok(())
}

pub fn set_capture_format_cb(
    f: FormatBuilder,
    visible_rect: v4l2r::Rect,
    min_num_buffers: usize,
) -> anyhow::Result<FormatChangedReply<PooledDMABufHandlesProvider>> {
    let format = f.set_pixelformat(b"RGBP").apply()?;

    println!(
        "New CAPTURE format: {:?} (visible rect: {})",
        format, visible_rect
    );

    let dri_file = Card::open("/dev/dri/card1")?;

    // Get the prefered mode for the first connector on the dri_file device
    let mode = mode_setting::get_mode(&dri_file)?;

    // Allocate n = min_num_buffers framebuffers with the size of the prefered mode and the format of the one defined above
    let frame_buffers: Vec<mode_setting::PrimeFramebuffer> = (0..min_num_buffers)
        .into_iter()
        .map(|_| mode_setting::get_framebuffer(&dri_file, &mode, &format).unwrap())
        .collect();

    // We need to set the crtc (screen) to display the first framebuffer
    let first_fb_handle = frame_buffers[0].handle;
    mode_setting::set_crtc(&dri_file, Some(first_fb_handle), Some(mode))?;

    // Get all the prime (drmbuf) RawFd (file descriptors) of the framebuffers we allocated
    let prime_file_descriptors: Vec<RawFd> = frame_buffers.into_iter().map(|fb| fb.prime).collect();

    // Register all these prime file descriptors with the v4l2 device
    //
    // This will return a vector of DMABufHandle<File> which we can make the provider of dma buffers
    let dmabuf_fds: Vec<Vec<_>> = dmabuf::register_dmabufs(
        &Path::new("/dev/dri/card1"),
        QueueType::VideoCaptureMplane,
        &format,
        &prime_file_descriptors,
    )
    .unwrap();

    Ok(FormatChangedReply {
        provider: PooledHandlesProvider::new(dmabuf_fds),
        // TODO: can't the provider report the memory type that it is
        // actually serving itself?
        mem_type: MemoryType::DMABuf,
        num_buffers: min_num_buffers,
    })
}

pub fn output_ready_cb(mut cap_dqbuf: DQBuffer<Capture, PooledHandles<DMABufferHandles<File>>>) {
    let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;
    // Ignore zero-sized buffers.
    if bytes_used == 0 {
        return;
    }

    print!(
        "\rDecoded buffer {:#5}, index: {:#2}), bytes used:{:#6}",
        cap_dqbuf.data.sequence(),
        cap_dqbuf.data.index(),
        bytes_used,
    );
    std::io::stdout().flush().unwrap();
}
pub fn decoder_event_cb(event: DecoderEvent<PooledDMABufHandlesProvider>) {
    match event {
        DecoderEvent::FrameDecoded(dqbuf) => output_ready_cb(dqbuf),
        DecoderEvent::EndOfStream => (),
    }
}
