#ifndef VMNET_HELPER_H
#define VMNET_HELPER_H

#include <vmnet/vmnet.h>
#include <stdint.h>

typedef struct {
    interface_ref iface;
    char mac[18];       // "xx:xx:xx:xx:xx:xx\0"
    uint64_t mtu;
    uint64_t max_pkt_size;
    int status;
} vmnet_iface_t;

int vmnet_helper_start(vmnet_iface_t *out);
void vmnet_helper_stop(vmnet_iface_t *iface);
int vmnet_helper_read(vmnet_iface_t *iface, void *buf, int buflen, int *pktlen);
int vmnet_helper_write(vmnet_iface_t *iface, void *buf, int len);
void vmnet_helper_set_event_callback(vmnet_iface_t *iface, void (*callback)(void *ctx), void *ctx);

#endif
