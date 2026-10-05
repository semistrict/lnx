// vmnet_helper.c — C wrapper for vmnet.framework's block-based API.
// Compiled by CGo alongside vmnet.go.

#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <string.h>
#include <stdlib.h>
#include "vmnet_helper.h"

int vmnet_helper_start(vmnet_iface_t *out) {
    memset(out, 0, sizeof(*out));

    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, VMNET_SHARED_MODE);

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block vmnet_return_t status = VMNET_FAILURE;
    __block vmnet_iface_t *result = out;

    out->iface = vmnet_start_interface(desc, dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0),
        ^(vmnet_return_t s, xpc_object_t params) {
            status = s;
            if (s == VMNET_SUCCESS && params) {
                const char *mac = xpc_dictionary_get_string(params, vmnet_mac_address_key);
                if (mac) strncpy(result->mac, mac, sizeof(result->mac) - 1);
                result->mtu = xpc_dictionary_get_uint64(params, vmnet_mtu_key);
                result->max_pkt_size = xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key);
            }
            dispatch_semaphore_signal(sem);
        });
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    xpc_release(desc);

    out->status = (int)status;
    return (status == VMNET_SUCCESS) ? 0 : -1;
}

void vmnet_helper_stop(vmnet_iface_t *iface) {
    if (!iface->iface) return;

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    vmnet_stop_interface(iface->iface, dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0),
        ^(vmnet_return_t s) {
            dispatch_semaphore_signal(sem);
        });
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    iface->iface = NULL;
}

int vmnet_helper_read(vmnet_iface_t *iface, void *buf, int buflen, int *pktlen) {
    struct iovec iov = { .iov_base = buf, .iov_len = buflen };
    struct vmpktdesc pkt = { .vm_pkt_size = buflen, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
    int cnt = 1;
    vmnet_return_t r = vmnet_read(iface->iface, &pkt, &cnt);
    if (r != VMNET_SUCCESS || cnt == 0) return -1;
    *pktlen = (int)pkt.vm_pkt_size;
    return 0;
}

int vmnet_helper_write(vmnet_iface_t *iface, void *buf, int len) {
    struct iovec iov = { .iov_base = buf, .iov_len = len };
    struct vmpktdesc pkt = { .vm_pkt_size = len, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
    int cnt = 1;
    vmnet_return_t r = vmnet_write(iface->iface, &pkt, &cnt);
    return (r == VMNET_SUCCESS) ? 0 : -1;
}

// Set up a dispatch source to call back when packets are available.
void vmnet_helper_set_event_callback(vmnet_iface_t *iface, void (*callback)(void *ctx), void *ctx) {
    dispatch_queue_t q = dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0);
    vmnet_interface_set_event_callback(iface->iface, VMNET_INTERFACE_PACKETS_AVAILABLE, q,
        ^(interface_event_t event, xpc_object_t params) {
            callback(ctx);
        });
}
