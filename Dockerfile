FROM rust

WORKDIR /app

ADD Cargo.toml Cargo.lock /app/
ADD src/lib.rs /app/src/lib.rs

RUN cargo build --release --lib

EXPOSE 80

ADD src /app/src/

RUN cargo build --release --bin riteaid-covid-rust

ENTRYPOINT [ "/app/target/release/riteaid-covid-rust" ]
