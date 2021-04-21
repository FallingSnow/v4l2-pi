Building for aarch64 (raspberry pi 4)
```
cargo build --target=aarch64-unknown-linux-gnu
```

Sync with remote raspberry pi
```
lsyncd -rsyncssh ~/Coding/rust/v4l2-pi-udp/src alarm@alarm.local /home/alarm/v4l2-pi-udp/src -nodaemon -delay 0.2
```

Due to linker errors in cross compiling, I currently just keep the src directory synced to my raspberry pi and use `cargo run` on the raspberry pi.

There are 3 folders.

##### src
Uses low level functions of the v4l2r library

##### stateful
Uses the stateful decoder portion of the v4l2r library

##### manual
Custom v4l2 bindings **Doesn't work/unfinished**