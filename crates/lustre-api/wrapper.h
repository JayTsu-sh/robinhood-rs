/*
 * wrapper.h — bindgen input for lustre-api.
 *
 * We include only the two user-facing Lustre headers. Bindgen produces Rust
 * definitions for every constant, enum, and struct reachable from these headers;
 * the `extern "C"` function declarations in src/sys.rs are hand-written for type
 * clarity and stable error messages, so we deliberately do NOT let bindgen
 * generate function signatures.
 */
#include <lustre/lustreapi.h>
#include <lustre/lustre_user.h>
#include <sys/ioctl.h>

enum { RBH_LL_IOC_HSM_REQUEST = LL_IOC_HSM_REQUEST };
