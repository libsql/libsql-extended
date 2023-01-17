# `sqld` User Guide

## Configuring gRPC to use TLS

Generate a CA private key:

```console
openssl genrsa -out ca.key 2048
```

and server private key:

```console
openssl genrsa -out server1.key 2048
```

Create certificates:

```console
cd cert && terraform init && terraform apply
```

Start a `sqld` server in primary mode:

```console
cargo run -- --grpc-listen-addr 127.0.0.1:5001 --grpc-tls --grpc-cert-file server1.pem --grpc-key-file server1.key
```

Start a `sqld` server in replica mode:

```console
cargo run -- --primary-grpc-url http://127.0.0.1:5001 --primary-grpc-tls --primary-grpc-cert-file ca.pem 
```

## Deploying to Fly

You can use the existing `fly.toml` file from this repository.

Just run
```console
flyctl launch
```
... then pick a name and respond "Yes" when the prompt asks you to deploy.

You now have `sqld` running on Fly listening for HTTP connections.

Give it a try with this snippet, replacing `$YOUR_APP` with your app name:
```
curl -X POST -d '{"statements": ["create table testme(a,b,c)"]}' $YOUR_APP.fly.dev
curl -X POST -d '{"statements": ["insert into testme values(1,2,3)"]}' $YOUR_APP.fly.dev
curl -X POST -d '{"statements": ["select * from testme"]}' $YOUR_APP.fly.dev
```
```
[{"b":2,"a":1,"c":3}]
```
