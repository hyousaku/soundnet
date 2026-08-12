/* Translation unit bindgen parses to produce this crate's FFI.
 *
 * Only the public headers the engine actually uses. roc's config.h, packet.h
 * and platform.h arrive transitively; listing them again would be harmless
 * but would suggest they were a deliberate part of the surface, which they
 * are not.
 *
 * Adding a header here widens the generated bindings. That is usually fine —
 * the allowlist in build.rs keeps the output to roc's own symbols — but every
 * addition is a promise that the ABI behind it is one we intend to depend on.
 */
#include <roc/context.h>
#include <roc/endpoint.h>
#include <roc/frame.h>
#include <roc/log.h>
#include <roc/metrics.h>
#include <roc/receiver.h>
#include <roc/sender.h>
#include <roc/version.h>
