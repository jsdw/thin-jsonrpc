# thin-jsonrpc-client

A lightweight wrapper around something that can send bytes and something that can hand them back which implements the JSON-RPC specification. The main goals of this are:

- To be backend independent; use whichever Websocket (or other) library you prefer.
- To be async runtime independent; use whatever you prefer.
- To support backpressure. You're handed back a "driver" which must be polled to drive receipt of messages; this can react to the messages (or errors) that come back, and rate limit by polling less frequently. Or, you can just run it in a task and forget about it if you don't care.
- To make it easy to access streams of server notifications (messages without ids attached). This streams can be used to build higher level logic on top, like handling for subscriptions and such.
