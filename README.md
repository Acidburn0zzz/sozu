# sozu HTTP proxy

it will be awesome when it will be ready

## Goals

## Building

### For OSX build

Mac OS uses an old version of openssl, so we need to use one from Homebrew:

```
brew install openssl
brew link --force openssl
```

If it does not work, set the following environment variables before building:

```
export OPENSSL_LIB_DIR=/usr/local/opt/openssl/lib/
export OPENSSL_INCLUDE_DIR=/usr/local/opt/openssl/include/
```

## Logging

The proxy uses `env_logger`. You can select which module displays logs at which level with an environment variable. Here is an example to display most logs at `info` level, but use `trace` level for the HTTP parser module:

```
RUST_LOG=info,sozu_lib::parser::http11=trace ./target/debug/sozu
```

## Exploring the source

- `lib/`: the `sozu_lib` proxy library contains the event loop management, the parsers and protocols
- `bin/`: the `sozu` executable wraps the library in worker processes, and handle dynamic configuration
- `ctl/`: the `sozuctl` executable can send commands to the proxy

## License

Copyright (C) 2015-2016 Geoffroy Couprie, Clément Delafargue

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, version 3.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU Affero General Public License for more details.
