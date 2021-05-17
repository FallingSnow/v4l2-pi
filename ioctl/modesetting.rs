use anyhow::Result;
use drm::{buffer::{Buffer, DrmFourcc, Handle, PlanarBuffer}, control::{Device, Mode, dumbbuffer::DumbBuffer, framebuffer}};
use std::os::unix::io::RawFd;

/// A simple wrapper for a device node.
#[derive(Debug)]
pub struct Card(std::fs::File);

/// Implementing `AsRawFd` is a prerequisite to implementing the traits found
/// in this crate. Here, we are just calling `as_raw_fd()` on the inner File.
impl std::os::unix::io::AsRawFd for Card {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// Simple helper methods for opening a `Card`.
impl Card {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        options.write(true);
        let card = Card(options.open(path)?);
        Ok(card)
    }
}

pub struct PrimeFramebuffer {
    pub handle: framebuffer::Handle,
    pub prime: RawFd,
}

pub struct PlanarDumbBuffer(DumbBuffer);

impl PlanarBuffer for PlanarDumbBuffer {
    fn size(&self) -> (u32, u32) {
        Buffer::size(&self.0)
    }

    fn format(&self) -> DrmFourcc {
        Buffer::format(&self.0)
    }

    fn pitches(&self) -> [u32; 4] {
        [self.0.pitch(), 0, 0, 0]
    }

    fn handles(&self) -> [Option<Handle>; 4] {
        [Some(self.0.handle()), None, None, None]
    }

    fn offsets(&self) -> [u32; 4] {
        [0; 4]
    }
}

impl From<DumbBuffer> for PlanarDumbBuffer {
    fn from(buffer: DumbBuffer) -> Self {
        Self(buffer)
    }
}

pub fn get_framebuffer(
    card: &Card,
    mode: &Mode,
    _format: &v4l2r::Format,
) -> Result<PrimeFramebuffer> {

    let pixel_format = DrmFourcc::Rgb565;
    // This should be 16 buf, but if we put 16 we get
    // videobuf2_common: [cap-0000000003662a70] __prepare_dmabuf: invalid dmabuf length 4149248 for plane 0, minimum length 4177920
    let bpp = 16;
    let dumb_buffer = card.create_dumb_buffer(
        (mode.size().0.into(), (mode.size().1 + 8).into()),
        pixel_format,
        bpp,
    )?;
    println!("New buffer size {:?}", dumb_buffer.size());
    let planar_dumb_buffer = PlanarDumbBuffer::from(dumb_buffer);

    let framebuffer = card
        .add_planar_framebuffer(&planar_dumb_buffer, &[None, None, None, None], 0)
        .unwrap();
    println!("Using {:?}", framebuffer);

    let prime_fd = card.buffer_to_prime_fd(dumb_buffer.handle(), 0)?;
    println!("Buffer prime FD {:?}", &prime_fd);

    Ok(PrimeFramebuffer {
        handle: framebuffer,
        prime: prime_fd,
    })
}

pub fn set_crtc(
    card: &Card,
    framebuffer: Option<framebuffer::Handle>,
    mode: Option<Mode>,
) -> Result<drm::control::crtc::Handle> {
    let resources = card.resource_handles()?;
    let connectors = resources.connectors();
    let connector_handle = connectors[0];
    let connector = card.get_connector(connector_handle)?;
    let curr_encoder = connector.current_encoder().unwrap();
    let encoder = card.get_encoder(curr_encoder)?;

    let crtc = encoder.crtc().unwrap();
    // println!("Using {:?}", crtc);

    card.set_crtc(crtc, framebuffer, (0, 0), &[connector_handle], mode)?;

    Ok(crtc)
}

pub fn get_mode(card: &Card) -> Result<Mode> {
    let resources = card.resource_handles()?;

    let connectors = resources.connectors();
    let connector_handle = connectors[0];
    let connector = card.get_connector(connector_handle)?;
    // let screen_size = connector.size().unwrap();
    let modes = card.get_modes(connector_handle)?;
    let mode = modes[0];

    println!("Handles:");
    connectors.iter().for_each(|h| println!("\t{:?}", h));

    println!(
        "- Connector: {:?} {:?}-{} {:?} {:?}",
        connector_handle,
        connector.interface(),
        connector.interface_id(),
        connector.state(),
        connector.size()
    );
    modes.iter().for_each(|m| println!("\t{:?}", m.name()));

    Ok(mode)
}
