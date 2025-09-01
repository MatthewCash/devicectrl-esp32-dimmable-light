# devicectrl-esp32-dimmable-light

Device implementation for a simple dimmable light running on an esp32c6.

Communication with the [server](https://github.com/MatthewCash/devicectrl-server) consists of ecdsa signed JSON messages, the ecdsa signing and verification is accelerated using the esp32's hardware ecc and sha blocks.

## Configuration

All configuration options are set at compile time by passing these environment variables to the `cargo build` command:

```sh
export SERVER_PUBLIC_KEY_PATH=/etc/ssl/certs/server_public.der
export PRIVATE_KEY_PATH=/etc/ssl/private/switch_private.der
export WIFI_SSID=wifi
export WIFI_PASSWORD=passw0rd
export IP_CIDR=10.0.2.10/24
```
