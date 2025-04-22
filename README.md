# Kafkrs

A Rust implementation of a Kafka like streaming platform.

## Components

### kafkrs-server

This is the main kafkrs crate, that runs the actual application. Currently it
only supports running a single instance,but can handle multiple connections but
only a single writer.

Going forward kafkrs will handle multiple topics, with multiple partitions,
each being handled by a distinct writer process.

### kafkrs-models

This crate holds all the shared models that are required across all other crates

### kafkrs-python

The kafkrs-python crate contains the PyO3 code to provide a python interface
to using kafkrs. Currently it supports generating valid messages that can be
sent to the server in a Python REPL for easy testing.

