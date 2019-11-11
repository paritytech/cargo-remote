# Server side setup

## 1 step

```Bash
nano /etc/profile
export PATH=/usr/local/cargo/bin:$PATH
export RUSTUP_HOME=/usr/local/rustup
export SCCACHE_IDLE_TIMEOUT=0
export SCCACHE_REDIS=redis://127.0.0.1/0
export RUSTC_WRAPPER=sccache
```

## 2 install rust

```Bash
wget https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init
chmod +x rustup-init; \
	./rustup-init -y --no-modify-path --default-toolchain stable; \
	rm rustup-init; \
	chmod -R a+w+r /usr/local/cargo/; \
	chmod -R a+w+r /usr/local/cargo/; \
	rustup install nightly beta; \
	rustup target add wasm32-unknown-unknown --toolchain nightly; \
	cargo install cargo-audit --force; \
	cargo install sccache --features redis --force; \
	cargo install --git https://github.com/alexcrichton/wasm-gc --force ;
```

## 3 redis

```Bash
protected mode no

maxmemory 50gb
maxmemory-policy allkeys-lru
service redis restart
```

<!--
## 4 user

export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_HOME=$HOME/.rustup
export CARGO_HOME=$HOME/.cargo

rustup install stable -->
