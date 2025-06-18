# Isotopia

Isotopia is a small build orchestrator which dispatches DantNIX ALARM iso build
requests to worker processes running on different machines.

# Usage

Isotopia can be build and executed with `cargo`:

```text
cargo run --release
```

When trying to upload new images, their base ALARM tarball checksum is validated
against the latest available tarball. It is recommended to disable this using
the `VALIDATE_TARBALL_MD5=0` environment variable during testing, since it can
generate significant traffic to `https://archlinuxarm.org`.
