use nix::{ioctl_none, ioctl_readwrite};

const DRM_DISPLAY_MODE_LEN: usize =	32;
const DRM_IOCTL_BASE: u8 =	b'd';

const DRM_IOCTL_SET_MASTER: u8 = 0x1e; // Defined in linux/spi/spidev.h
const DRM_IOCTL_DROP_MASTER: u8 = 0x1f;
const DRM_IOCTL_MODE_GETRESOURCES: u8 = 0xA0;
const DRM_IOCTL_MODE_GETCONNECTOR: u8 = 0xA7;

#[derive(Debug)]
#[repr(C)]
pub struct DrmModeCardRes {
	 pub fb_id_ptr: *mut u32,
	 pub crtc_id_ptr: *mut u32,
	 pub connector_id_ptr: *mut u32,
	 pub encoder_id_ptr: *mut u32,
	 pub count_fbs: u32,
	 pub count_crtcs: u32,
	 pub count_connectors: u32,
	 pub count_encoders: u32,
	 pub min_width: u32,
	 pub max_width: u32,
	 pub min_height: u32,
	 pub max_height: u32,
}

impl Default for DrmModeCardRes {
	fn default() -> Self {
			Self {
				fb_id_ptr: std::ptr::null_mut(),
				crtc_id_ptr: std::ptr::null_mut(),
				connector_id_ptr: std::ptr::null_mut(),
				encoder_id_ptr: std::ptr::null_mut(),
				count_fbs: 0,
				count_crtcs: 0,
				count_connectors: 0,
				count_encoders: 0,
				min_width: 0,
				max_width: 0,
				min_height: 0,
				max_height: 0,
		 }
	}
}

#[derive(Debug)]
#[repr(C)]
pub struct DrmModeGetConnector {
	pub encoders_ptr: *mut u32,
	pub modes_ptr: *mut DrmModeModeinfo,
	pub props_ptr: *mut u32,
	pub prop_values_ptr: *mut u32,
	pub count_modes: u32,
	pub count_props: u32,
	pub count_encoders: u32,
	pub encoder_id: u32, /**< Current Encoder */
	pub connector_id: u32, /**< Id */
	pub connector_type: u32,
	pub connector_type_id: u32,
	pub connection: u32,
	pub mm_width: u32,  /**< width in millimeters */
	pub mm_height: u32, /**< height in millimeters */
	pub subpixel: u32,
	pub pad: u32,
}

impl Default for DrmModeGetConnector {
	fn default() -> Self {
			Self {
				encoders_ptr: std::ptr::null_mut(),
				modes_ptr: std::ptr::null_mut(),
				props_ptr: std::ptr::null_mut(),
				prop_values_ptr: std::ptr::null_mut(),
				count_modes: 0,
				count_props: 0,
				count_encoders: 0,
				encoder_id: 0, /**< Current Encoder */
				connector_id: 0, /**< Id */
				connector_type: 0,
				connector_type_id: 0,
				connection: 0,
				mm_width: 0,  /**< width in millimeters */
				mm_height: 0, /**< height in millimeters */
				subpixel: 0,
				pad: 0,
		 }
	}
}

#[derive(Debug, Default, Clone)]
#[repr(C)]
pub struct DrmModeModeinfo {
	pub clock: u32,
	pub hdisplay: u16,
	pub hsync_start: u16,
	pub hsync_end: u16,
	pub htotal: u16,
	pub hskew: u16,
	pub vdisplay: u16,
	pub vsync_start: u16,
	pub vsync_end: u16,
	pub vtotal: u16,
	pub vscan: u16,
	pub vrefresh: u32,
	pub flags: u32,
	pub type_: u32,
	pub name: [char; DRM_DISPLAY_MODE_LEN],
}

ioctl_none!(drm_set_master, DRM_IOCTL_BASE, DRM_IOCTL_SET_MASTER);
ioctl_none!(drm_drop_master, DRM_IOCTL_BASE, DRM_IOCTL_DROP_MASTER);
ioctl_readwrite!(drm_mode_getresources, DRM_IOCTL_BASE, DRM_IOCTL_MODE_GETRESOURCES, DrmModeCardRes);
ioctl_readwrite!(drm_mode_getconnector, DRM_IOCTL_BASE, DRM_IOCTL_MODE_GETCONNECTOR, DrmModeGetConnector);