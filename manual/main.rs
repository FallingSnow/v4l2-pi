mod dmabuf;
mod mode_setting;
mod v4l2;

fn main() -> anyhow::Result<()> {
    let decoder = v4l2::init_decode()?;

    let file = std::fs::File::open("/home/alarm/FPS_test_1080p60_L4.2.h264")?;

    v4l2::decode(decoder, file)?;

    Ok(())
}
