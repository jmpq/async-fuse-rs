use std::env;
use async_fuse::Filesystem;

struct NullFS;

impl Filesystem for NullFS {}

#[tokio::main]
async fn main() {
    env_logger::init();
    let mountpoint = env::args_os().nth(1).unwrap();
    async_fuse::mount(NullFS, mountpoint, &[]).await.unwrap();
}
