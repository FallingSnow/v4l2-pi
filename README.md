Building for aarch64 (raspberry pi 4)
```
cargo build --target=aarch64-unknown-linux-gnu
```

Sync with remote raspberry pi
```
lsyncd -rsyncssh ~/Coding/rust/v4l2-pi-udp/src alarm@alarm.local /home/alarm/v4l2-pi-udp/src -nodaemon -delay 0
```