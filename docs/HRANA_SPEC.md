# The Hrana protocol specification

Hrana (from Czech "hrana", which means "edge") is a protocol for connecting to
SQL database over a WebSocket. It is designed to be used from edge functions,
where low latency and small overhead is important.

## Motivation

This protocol aims to provide several benefits over the Postgres wire protocol:

- Works in edge runtimes: WebSockets are available in all edge runtimes
(Cloudflare Workers, Deno Deploy, Lagon), but general TCP sockets are not
(notably, sockets are not supported by Cloudflare Workers).

- Fast cold start: the Postgres wire protocol requires [at least two
roundtrips][pgwire-flow] before the client can send queries, but Hrana needs
just a single roundtrip introduced by the WebSocket protocol. (In both cases,
additional roundtrips might be necessary due to TLS.)

- Multiplexing: a single Hrana connection can open multiple SQL streams, so an
application needs to open just a single connection even if it handles multiple
concurrent requests.

- Simplicity: Hrana is a simple protocol, so a client needs few lines of
code. This is important on edge runtimes that impose hard limits on code size
(usually just a few MB).

[pgwire-flow]: https://www.postgresql.org/docs/current/protocol-flow.html

## Usage

The Hrana protocol is intended to be used in one of two ways:

- Connecting to `sqld`: edge functions and other clients can connect directly
to `sqld` using Hrana, because it has native support for the protocol. This is
the approach with lowest latency, because no software in the middle is
necessary.

- Connecting to Postgres or SQLite through a proxy: this allows edge functions
to efficiently connect to existing SQL databases.

## Overview

The protocol runs on top of the [WebSocket protocol][rfc6455] as a subprotocol
`hrana1`. The client includes `hrana1` in the `Sec-WebSocket-Protocol` request
header in the opening handshake, and the server replies with `hrana1` in the
same response header. Future versions of the Hrana protocol will be negotiated
as different WebSocket subprotocols.

[rfc6455]: https://www.rfc-editor.org/rfc/rfc6455

The client starts the connection by sending a _hello_ message, which
authenticates the client to the server. The server responds with either a
confirmation or with an error message, closing the connection. The client can
choose not to wait for the confirmation and immediately send further messages to
reduce latency.

A single connection can host an arbitrary number of _streams_. A stream
corresponds to a "session" in PostgreSQL or a "connection" in SQLite: SQL
statements in a stream are executed sequentially and can affect stream-specific
state such as transactions (with SQL `BEGIN` or `SAVEPOINT`). In effect, one
Hrana connection works as a "connection pool" in traditional SQL servers.

After a stream is opened, the client can execute SQL _statements_ on it. For the
purposes of this protocol, the statements are arbitrary strings with optional
parameters. The protocol can thus work with any SQL dialect.

To reduce the number of roundtrips, the protocol supports rudimentary
computation on the server, which can be used to conditionally execute
statements. For example, this mechanism can be used to implement non-interactive
transactions (batches).

## Messages

All messages exchanged between the client and server are text messages encoded
in JSON. Future versions of the protocol might additionally support binary
messages with a more compact binary encoding.

This specification describes the JSON messages using TypeScript syntax as
follows:

```
type ClientMsg =
    | HelloMsg
    | RequestMsg

type ServerMsg =
    | HelloOkMsg
    | HelloErrorMsg
    | ResponseOkMsg
    | ResponseErrorMsg
```

The client sends messages of type `ClientMsg`, and the server sends messages of
type `ServerMsg`. The type of the message is determined by its `type` field.

### Hello

```
type HelloMsg = {
    "type": "hello",
    "jwt": string | null,
}
```

The `hello` message is sent as the first message by the client. It authenticates
the client to the server using the [Json Web Token (JWT)][rfc7519] passed in the
`jwt` field. If no authentication is required (which might be useful for
development and debugging, or when authentication is performed by other means,
such as with mutual TLS), the `jwt` field might be set to `null`.

[rfc7519]: https://www.rfc-editor.org/rfc/rfc7519

```
type HelloOkMsg = {
    "type": "hello_ok",
}

type HelloErrorMsg = {
    "type": "hello_error",
    "error": Error,
}
```

The server waits for the `hello` message from the client and responds with a
`hello_ok` message if the client can proceed, or with a `hello_error` message
describing the failure.

The client may choose not to wait for a response to its `hello` message before
sending more messages to save a network roundtrip. If the server responds with
`hello_error`, it must ignore all further messages sent by the client and it
should close the WebSocket immediately.

### Request/response

```
type RequestMsg = {
    "type": "request",
    "request_id": int32,
    "request": Request,
}
```

After sending the `hello` message, the client can start sending `request`
messages. The client uses requests to open SQL streams and execute statements on
them. The client assigns an identifier to every request, which is then used to
match a response to the request.

```
type ResponseOkMsg = {
    "type": "response_ok",
    "request_id": int32,
    "response": Response,
}

type ResponseErrorMsg = {
    "type": "response_error",
    "request_id": int32,
    "error": Error,
}
```

When the server receives a `request` message, it must eventually send either a
`response_ok` with the response or a `response_error` that describes a failure.
The response from the server includes the same `request_id` that was provided by
the client in the request. The server can send the responses in arbitrary order.

The request ids are arbitrary 32-bit signed integers, the server does not
interpret them in any way.

The server should limit the number of outstanding requests to a reasonable
value, and stop receiving messages when this limit is reached. This will cause
the TCP flow control to kick in and apply back-pressure to the client. On the
other hand, the client should always receive messages, to avoid deadlock.

### Errors

```
type Error = {
    "message": string,
}
```

When a server refuses to accept a client `hello` or fails to process a
`request`, it responds with a message that describes the error. The `message`
field contains an English human-readable description of the error. The protocol
will be extended with machine-readable error codes in the future.

If either peer detects that the protocol has been violated, it should close the
WebSocket with an appropriate WebSocket close code and reason. Some examples of
protocol violations include:

- Text message that is not a valid JSON.
- Unrecognized `ClientMsg` or `ServerMsg` (the field `type` is unknown or
missing)
- Client receives a `ResponseOkMsg` or `ResponseErrorMsg` with a `request_id`
that has not been sent in a `RequestMsg` or that has already received a
response.

## Requests

Most of the work in the protocol happens in request/response interactions.

```
type Request =
    | OpenStreamReq
    | CloseStreamReq
    | ComputeReq
    | ExecuteReq

type Response =
    | OpenStreamResp
    | CloseStreamResp
    | ComputeResp
    | ExecuteResp
```

The type of the request and response is determined by its `type` field. The
`type` of the response must always match the `type` of the request.

### Open stream

```
type OpenStreamReq = {
    "type": "open_stream",
    "stream_id": int32,
}

type OpenStreamResp = {
    "type": "open_stream",
}
```

The client uses the `open_stream` request to open an SQL stream, which is then
used to execute SQL statements. The streams are identified by arbitrary 32-bit
signed integers assigned by the client.

The client can optimistically send follow-up requests on a stream before it
receives the response to its `open_stream` request. If the server receives a
request that refers to a stream that failed to open, it should respond with an
error, but it should not close the connection.

Even if the `open_stream` request returns an error, the stream id is still
considered as used, and the client cannot reuse it until it sends a
`close_stream` request.

The server can impose a reasonable limit to the number of streams opened at the
same time.

### Close stream

```
type CloseStreamReq = {
    "type": "close_stream",
    "stream_id": int32,
}

type CloseStreamResp = {
    "type": "close_stream",
}
```

When the client is done with a stream, it should close it using the
`close_stream` request. The client can safely reuse the stream id after it
receives the response.

The client should close even streams for which the `open_stream` request
returned an error.

### Evaluate compute operations

```
type ComputeReq = {
    "type": "compute",
    "ops": Array<ComputeOp>,
}

type ComputeResp = {
    "type": "compute",
    "results": Array<Value>,
}
```

The `compute` request can be used to evaluate compute operations on the server.
The operations are evaluated sequentially, and the response contains the result
of each operation.

### Execute a statement

```
type ExecuteReq = {
    "type": "execute",
    "stream_id": int32,
    "stmt": Stmt,
    "condition"?: ComputeExpr | null,
    "on_ok"?: Array<ComputeOp>,
    "on_error"?: Array<ComputeOp>,
}

type ExecuteResp = {
    "type": "execute",
    "result": StmtResult | null,
}
```

The client sends an `execute` request to execute an SQL statement on a stream.
The server responds with the result of the statement.

If the request contains the `condition`, the statement will be executed only if
it evaluates to true. If the condition evaluates to false, the response `result`
will be `null`.

If the statement executes successfully, the server evaluates operations in
`on_ok`. If the statement fails with an error, the server evaluates operations
in `on_error`. If the statement is skipped because `condition` evaluated to
false, neither `on_ok` nor `on_error` is evaluated. The results of these
operations are ignored.

```
type Stmt = {
    "sql": string,
    "args"?: Array<Value>,
    "named_args"?: Array<NamedArg>,
    "want_rows": boolean,
}

type NamedArg = {
    "name": string,
    "value": Value,
}
```

A statement contains the SQL text in `sql` and arguments.

The arguments in `args` are bound to positional parameters in the SQL statement
(such as `$NNN` in Postgres or `?NNN` in SQLite). The arguments in `named_args`
are bound to named arguments, such as `:AAAA`, `@AAAA` and `$AAAA` in SQLite.

For SQLite, the names of arguments include the prefix sign (`:`, `@` or `$`). If
the name of the argument does not start with this prefix, the server will try to
guess the correct prefix. If an argument is specified both as a positional
argument and as a named argument, the named argument should take precedence.

It is an error if the request specifies an argument that is not expected by the
SQL statement, or if the request does not specify and argument that is expected
by the SQL statement. Some servers may not support specifying both positional
and named arguments.

The `want_rows` field specifies whether the client is interested in the rows
produced by the SQL statement. If it is set to `false`, the server should always
reply with no rows, even if the statement produced some.

The SQL text should contain just a single statement. Issuing multiple statements
separated by a semicolon is not supported.

```
type StmtResult = {
    "cols": Array<Col>,
    "rows": Array<Array<Value>>,
    "affected_row_count": int32,
    "last_insert_rowid": string | null,
}

type Col = {
    "name": string | null,
}
```

The result of executing an SQL statement contains information about the returned
columns in `cols` and the returned rows in `rows` (the array is empty if the
statement did not produce any rows or if `want_rows` was `false` in the request).

`affected_row_count` counts the number of rows that were changed by the
statement. This is meaningful only if the statement was an INSERT, UPDATE or
DELETE, and the value is otherwise undefined.

`last_insert_rowid` is the ROWID of the last successful insert into a rowid
table. The rowid value is a 64-bit signed integer encoded as a string. For
other statements, the value is undefined.

### Values

```
type Value =
    | { "type": "null" }
    | { "type": "integer", "value": string }
    | { "type": "float", "value": number }
    | { "type": "text", "value": string }
    | { "type": "blob", "base64": string }
```

Values passed as arguments to SQL statements and returned in rows are one of
supported types:

- `null`: the SQL NULL value
- `integer`: a 64-bit signed integer, its `value` is a string to avoid losing
precision, because some JSON implementations treat all numbers as 64-bit floats
- `float`: a 64-bit float
- `text`: a UTF-8 text string
- `blob`: a binary blob with base64-encoded value

These types exactly correspond to SQLite types. In the future, the protocol
might be extended with more types for compatibility with Postgres.

### Compute operations

```
type ComputeOp =
    | { "type": "set", "var": int32, "expr": ComputeExpr }
    | { "type": "unset", "var": int32 }
    | { "type": "eval", "expr": ComputeExpr }
```

Compute operations are sequential instructions. They can refer to variables,
which are named with arbitrary integers assigned by the client. Each operation
produces a value as a result; this result is returned by the `compute` request,
for example.

- `set` evaluates an expression and assigns it to a variable. The variable is
created if it does not exist. The result of this op is NULL.
- `unset` removes a variable. The client should unset unused variables to save
resources on the server. The result of this op is NULL.
- `eval` evaluates an expression and returns the result.

```
type ComputeExpr =
    | Value
    | { "type": "var", "var": int32 }
```

Expressions evaluate to values. Expressions are pure, their evaluation does not
have side effects.

- Each `Value` is also an expression that evaluates to itself.
- `var` evaluates to the current value of given variable. If the variable is not
set, an error is returned.

When a value is treated as a boolean (such as in the condition of `execute`
request), they are converted as follows:

- NULL is false.
- Integers and floats are true iff they are nonzero.
- Texts and blobs are true iff they are nonempty.

### Ordering

The protocol allows the server to reorder the responses: it is not necessary to
send the responses in the same order as the requests. However, the server must
process requests related to a single stream id in order. The server also
evaluates `compute` requests and conditions on `execute` requests in order.

For example, this means that a client can send an `open_stream` request
immediately followed by a batch of `execute` requests on that stream and the
server will always process them in correct order.
