mod device;
mod event_handler;

pub use self::device::Pmem;

mod defs {
    use crate::virtio::QueueConfig;

    pub const NUM_QUEUES: usize = 1;
    pub const QUEUE_SIZE: u16 = 128;
    pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

    pub mod uapi {
        pub const VIRTIO_F_VERSION_1: u32 = 32;
        pub const VIRTIO_ID_PMEM: u32 = 27;
        pub const VIRTIO_PMEM_REQ_TYPE_FLUSH: u32 = 0;
    }
}
