#include <Hypervisor/Hypervisor.h>

hv_return_t krun_hv_vcpu_set_simd_fp_reg_from_bytes(
    hv_vcpu_t vcpu,
    hv_simd_fp_reg_t reg,
    const unsigned char value_bytes[16])
{
    hv_simd_fp_uchar16_t value;
    __builtin_memcpy(&value, value_bytes, sizeof(value));
    return hv_vcpu_set_simd_fp_reg(vcpu, reg, value);
}
