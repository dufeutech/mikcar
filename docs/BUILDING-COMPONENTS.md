# Building WASI HTTP Components

This guide explains how to build `wasi:http/incoming-handler@0.2.0` components in different languages that can run on mik or any WASI HTTP runtime.

## Overview

mik uses **WASI HTTP 0.2.0**. When building components, use matching WIT definitions.

| Language   | SDK/Toolchain                                                            | Raw Size | Stripped |
| ---------- | ------------------------------------------------------------------------ | -------- | -------- |
| C/C++      | [wasi-sdk](https://github.com/WebAssembly/wasi-sdk) + wit-bindgen        | ~88 KB   | ~25 KB   |
| Rust       | [wit-bindgen](https://github.com/bytecodealliance/wit-bindgen) / mik-sdk | ~100 KB  | ~100 KB  |
| Go         | [wasi-go-sdk](https://github.com/rajatjindal/wasi-go-sdk) + TinyGo       | ~1.3 MB  | ~304 KB  |
| JavaScript | [jco](https://github.com/bytecodealliance/jco) (ComponentizeJS)          | ~12 MB   | ~12 MB   |
| Python     | [componentize-py](https://github.com/bytecodealliance/componentize-py)   | ~39 MB   | ~17 MB   |

> **Tip:** Use `wasm-tools strip --all component.wasm -o stripped.wasm` to remove debug info and names.

## Prerequisites

```bash
# C/C++ (wasi-sdk 25+)
# Download from https://github.com/WebAssembly/wasi-sdk/releases
cargo install wit-bindgen-cli  # For generating C bindings

# Rust
rustup target add wasm32-wasip2
cargo install cargo-component wasm-tools

# Go (TinyGo 0.33+)
# Download from https://tinygo.org/getting-started/install/

# JavaScript
npm install -g @bytecodealliance/jco @bytecodealliance/componentize-js

# Python
pip install componentize-py
```

---

## C/C++

C produces the smallest components using [wasi-sdk](https://github.com/WebAssembly/wasi-sdk).

### Setup

```bash
mkdir hello-c && cd hello-c

# Get WASI HTTP 0.2.0 WIT
git clone --depth 1 --branch v0.2.0 https://github.com/WebAssembly/wasi-http.git

# Generate C bindings
wit-bindgen c wasi-http/wit --out-dir gen --world proxy
```

### Code

```c
// hello.c
#include "gen/proxy.h"
#include <string.h>

static const char* RESPONSE_BODY = "{\"message\":\"Hello from C!\",\"lang\":\"c\"}";

void exports_wasi_http_incoming_handler_handle(
    exports_wasi_http_incoming_handler_own_incoming_request_t request,
    exports_wasi_http_incoming_handler_own_response_outparam_t response_out
) {
    wasi_http_types_incoming_request_drop_own(request);

    // Create response
    wasi_http_types_own_fields_t headers = wasi_http_types_constructor_fields();
    wasi_http_types_own_outgoing_response_t response =
        wasi_http_types_constructor_outgoing_response(headers);

    wasi_http_types_borrow_outgoing_response_t resp_borrow =
        (wasi_http_types_borrow_outgoing_response_t){ response.__handle };
    wasi_http_types_method_outgoing_response_set_status_code(resp_borrow, 200);

    // Get body and stream
    wasi_http_types_own_outgoing_body_t body;
    wasi_http_types_method_outgoing_response_body(resp_borrow, &body);

    wasi_io_streams_own_output_stream_t stream;
    wasi_http_types_borrow_outgoing_body_t body_borrow =
        (wasi_http_types_borrow_outgoing_body_t){ body.__handle };
    wasi_http_types_method_outgoing_body_write(body_borrow, &stream);

    // Set response before writing
    wasi_http_types_result_own_outgoing_response_error_code_t result = {
        .is_err = false, .val.ok = response
    };
    wasi_http_types_static_response_outparam_set(response_out, &result);

    // Write body
    proxy_list_u8_t body_bytes = { (uint8_t*)RESPONSE_BODY, strlen(RESPONSE_BODY) };
    wasi_io_streams_stream_error_t err;
    wasi_io_streams_borrow_output_stream_t stream_borrow =
        (wasi_io_streams_borrow_output_stream_t){ stream.__handle };
    wasi_io_streams_method_output_stream_blocking_write_and_flush(stream_borrow, &body_bytes, &err);

    wasi_io_streams_output_stream_drop_own(stream);
    wasi_http_types_error_code_t body_err;
    wasi_http_types_static_outgoing_body_finish(body, NULL, &body_err);
}
```

### Build

```bash
# Set WASI_SDK to your installation path
WASI_SDK=/path/to/wasi-sdk

$WASI_SDK/bin/clang --target=wasm32-wasip2 -O2 \
    -c gen/proxy.c -o proxy.o
$WASI_SDK/bin/clang --target=wasm32-wasip2 -O2 \
    -c hello.c -o hello.o
$WASI_SDK/bin/clang --target=wasm32-wasip2 -O2 \
    proxy.o hello.o gen/proxy_component_type.o -o hello-c.wasm
```

---

## Rust

Rust produces compact components. Two options:

### Option 1: mik-sdk (Recommended)

```bash
cargo init my-handler && cd my-handler
mik init
mik add mik-sdk/router
```

```rust
// src/lib.rs
use mik_sdk::prelude::*;

#[handler]
fn handle(req: Request) -> Response {
    Response::json(&serde_json::json!({
        "message": "Hello from Rust!",
        "lang": "rust"
    }))
}
```

```bash
mik build --release --compose
# Output: target/wasm32-wasip2/release/composed.wasm
```

### Option 2: wit-bindgen (Direct)

```bash
cargo new --lib my-component && cd my-component
cargo add wit-bindgen
```

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]
```

```bash
cargo component build --release
```

---

## Go

Uses [wasi-go-sdk](https://github.com/rajatjindal/wasi-go-sdk) with TinyGo.

### Setup

```bash
mkdir hello-go && cd hello-go
go mod init hello-go
go get github.com/rajatjindal/wasi-go-sdk@latest
```

### Code

```go
// main.go
package main

import (
    "net/http"
    "github.com/rajatjindal/wasi-go-sdk/pkg/wasihttp"
)

func init() {
    wasihttp.Handle(func(w http.ResponseWriter, r *http.Request) {
        w.Header().Set("Content-Type", "application/json")
        w.WriteHeader(http.StatusOK)
        w.Write([]byte(`{"message":"Hello from Go!","lang":"go"}`))
    })
}

func main() {}
```

### Build

```bash
# Get WIT from SDK
WIT_DIR=$(go list -m -f '{{.Dir}}' github.com/rajatjindal/wasi-go-sdk)/wit

# Build
tinygo build -target=wasip2 --wit-package "$WIT_DIR" --wit-world sdk -o hello-go.wasm .
```

---

## JavaScript

Uses [jco](https://github.com/bytecodealliance/jco) (ComponentizeJS).

### Setup

```bash
mkdir hello-js && cd hello-js

# Get WASI HTTP 0.2.0 WIT
git clone --depth 1 --branch v0.2.0 https://github.com/WebAssembly/wasi-http.git
mkdir -p wit && cp -r wasi-http/wit/deps wit/ && cp wasi-http/proxy.wit wit/
```

```wit
// wit/world.wit
package test:hello;

world hello {
  export wasi:http/incoming-handler@0.2.0;
}
```

### Code

```javascript
// app.js
import {
  ResponseOutparam, OutgoingBody, OutgoingResponse, Fields,
} from 'wasi:http/types@0.2.0';

export const incomingHandler = {
  handle(_incomingRequest, responseOutparam) {
    const response = new OutgoingResponse(new Fields());
    response.setStatusCode(200);

    let body = response.body();
    let stream = body.write();
    stream.blockingWriteAndFlush(
      new Uint8Array(new TextEncoder().encode(
        JSON.stringify({ message: "Hello from JavaScript!", lang: "javascript" })
      ))
    );
    stream[Symbol.dispose]();
    OutgoingBody.finish(body, undefined);
    ResponseOutparam.set(responseOutparam, { tag: 'ok', val: response });
  }
};
```

### Build

```bash
jco componentize app.js --wit wit --world-name hello -o hello-js.wasm
```

---

## Python

Uses [componentize-py](https://github.com/bytecodealliance/componentize-py).

### Setup

```bash
mkdir hello-python && cd hello-python

# Get WASI HTTP 0.2.0 WIT
git clone --depth 1 --branch v0.2.0 https://github.com/WebAssembly/wasi-http.git
mkdir -p wit && cp -r wasi-http/wit/deps wit/ && cp wasi-http/proxy.wit wit/
```

```wit
// wit/world.wit
package test:hello;

world hello {
  export wasi:http/incoming-handler@0.2.0;
}
```

### Generate Bindings

```bash
componentize-py bindings wit --world hello
```

### Code

```python
# app.py
from wit_world import exports
from componentize_py_types import Ok
from wit_world.imports.types import (
    IncomingRequest, ResponseOutparam, OutgoingResponse, Fields, OutgoingBody
)

class IncomingHandler(exports.IncomingHandler):
    def handle(self, _: IncomingRequest, response_out: ResponseOutparam):
        response = OutgoingResponse(Fields.from_list([
            ("content-type", b"application/json"),
        ]))
        response.set_status_code(200)
        body = response.body()

        # Set response BEFORE writing body
        ResponseOutparam.set(response_out, Ok(response))

        body.write().blocking_write_and_flush(
            b'{"message":"Hello from Python!","lang":"python"}'
        )
        OutgoingBody.finish(body, None)
```

### Build

```bash
componentize-py componentize app -d wit -w hello -o hello-python.wasm
```

---

## Verify Component

```bash
# Check exports
wasm-tools component wit my-component.wasm | grep "export wasi:http"
# export wasi:http/incoming-handler@0.2.0

# Run with mik
mik run my-component.wasm

# Test
curl http://localhost:3000/
```

---

## Troubleshooting

| Issue                                              | Solution                                          |
| -------------------------------------------------- | ------------------------------------------------- |
| `module requires import interface wasi:http/types` | Use wasi-http v0.2.0 WIT (not latest)             |
| `ResponseOutparam.set is not a function` (JS)      | Use `{ tag: 'ok', val: response }`                |
| `missing Ok wrapper` (Python)                      | Import `Ok` from `componentize_py_types`          |
| Empty response                                     | Call `ResponseOutparam.set()` BEFORE writing body |

## Version Compatibility

| mik   | wasmtime | wasi-http WIT |
| ----- | -------- | ------------- |
| 0.0.1 | 40.0.0   | 0.2.0         |
