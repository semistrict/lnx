use std::os::unix::io::AsRawFd;

use polly::event_manager::{EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

use super::device::{Pmem, REQ_INDEX};
use crate::virtio::device::VirtioDevice;

impl Pmem {
    pub(crate) fn handle_req_event(&mut self, event: &EpollEvent) {
        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("pmem: request queue unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(REQ_INDEX).read() {
            error!("pmem: failed to read request queue event: {e:?}");
        } else if self.process_req() {
            self.device_state.signal_used_queue();
        }
    }

    fn handle_activate_event(&self, event_manager: &mut EventManager) {
        if let Err(e) = self.activate_evt.read() {
            error!("pmem: failed to consume activate event: {e:?}");
        }

        let self_subscriber = event_manager
            .subscriber(self.activate_evt.as_raw_fd())
            .unwrap();

        event_manager
            .register(
                self.queue_event(REQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(REQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("pmem: failed to register request queue: {e:?}");
            });

        event_manager
            .unregister(self.activate_evt.as_raw_fd())
            .unwrap_or_else(|e| {
                error!("pmem: failed to unregister activate event: {e:?}");
            })
    }
}

impl Subscriber for Pmem {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let activate_evt = self.activate_evt.as_raw_fd();
        if source == activate_evt {
            self.handle_activate_event(event_manager);
        } else if self.is_activated() {
            let req = self.queue_event(REQ_INDEX).as_raw_fd();
            if source == req {
                self.handle_req_event(event);
            } else {
                warn!("pmem: unexpected event received: {source:?}");
            }
        } else {
            warn!("pmem: spurious event before activation: {source:?}");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.activate_evt.as_raw_fd() as u64,
        )]
    }
}
