use std::{
    collections::VecDeque,
    fs::File,
    os::unix::io::{FromRawFd, RawFd},
    path::Path,
};

use v4l2r::{
    device::Device,
    ioctl::{self},
    memory::{DmaBufHandle, MemoryType},
    Format, QueueType,
};

use anyhow::Result;

pub fn register_dmabufs(
    device_path: &Path,
    queue: QueueType,
    format: &Format,
    file_descriptors: &[RawFd],
) -> Result<VecDeque<Vec<DmaBufHandle<File>>>> {
    let mut device = Device::open(device_path, Default::default())?;

    let set_format: Format = ioctl::s_fmt(&mut device, queue, format.clone()).unwrap();
    if set_format != *format {
        eprintln!("Requested format does not apply as-is");
        eprintln!("Requested format: {:?}", format);
        eprintln!("Applied format: {:?}", format);
        return Err(anyhow::anyhow!("Could not apply requested format"));
    }

    // Request some space be allocated for the buffers we are able to register
    let nb_buffers: usize = ioctl::reqbufs(
        &device,
        queue,
        MemoryType::DmaBuf,
        file_descriptors.len() as u32,
    )
    .unwrap();
    assert_eq!(file_descriptors.len(), nb_buffers);

    let fds: VecDeque<Vec<DmaBufHandle<File>>> = (0..nb_buffers)
        .into_iter()
        .map(|buffer| {
            // let plane_buffers = [file_descriptors[buffer]];

            // Register buffer with v4l2 driver
            // querybuf::<ioctl::QueryBuffer, Device>(&device, queue, format, buffer, &plane_buffers)
            //     .unwrap();

            (0..format.plane_fmt.len())
                .into_iter()
                .map(move |_| {
                    let buffer_file: File = unsafe { File::from_raw_fd(file_descriptors[buffer]) };
                    DmaBufHandle::from(buffer_file)
                })
                .collect()
        })
        .collect();

    // We can close the device now, the registered buffers will remain alive as
    // long as they are referenced.
    drop(device);

    Ok(fds)
}

// type PlaneData = [bindings::v4l2_plane; bindings::VIDEO_MAX_PLANES as usize];

// fn is_multi_planar(queue: QueueType) -> bool {
//     queue == QueueType::VideoCaptureMplane || queue == QueueType::VideoOutputMplane
// }

// nix::ioctl_readwrite!(vidioc_querybuf, b'V', 9, bindings::v4l2_buffer);

// /// Safe wrapper around the `VIDIOC_QUERYBUF` ioctl.
// /// Use this function to register a buffer with the v4l2 driver or to get information on an already registered buffer
// pub fn querybuf<T: QueryBuf, F: AsRawFd>(
//     fd: &F,
//     queue: QueueType,
//     format: &Format,
//     index: usize,
//     file_descriptors: &[RawFd],
// ) -> Result<T, QueryBufError> {
//     let mut v4l2_buf = bindings::v4l2_buffer {
//         index: index as u32,
//         type_: queue as u32,
//         ..unsafe { mem::zeroed() }
//     };

//     if is_multi_planar(queue) {
//         let mut plane_data: PlaneData = Default::default();

//         for (i, &fd) in file_descriptors.iter().enumerate() {
//             plane_data[i].length = format.plane_fmt[i].sizeimage;
//             plane_data[i].m.fd = fd;
//         }

//         v4l2_buf.m.planes = plane_data.as_mut_ptr();
//         v4l2_buf.length = plane_data.len() as u32;

//         unsafe { vidioc_querybuf(fd.as_raw_fd(), &mut v4l2_buf) }?;
//         Ok(T::from_v4l2_buffer(&v4l2_buf, Some(&plane_data)))
//     } else {
//         v4l2_buf.m.fd = file_descriptors[0];

//         unsafe { vidioc_querybuf(fd.as_raw_fd(), &mut v4l2_buf) }?;
//         Ok(T::from_v4l2_buffer(&v4l2_buf, None))
//     }
// }
