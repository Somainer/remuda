## Host Claude login inventory

`remuda doctor` and host `cli[]` report whether Claude is installed and
whether a native login or API gateway is configured. `auth` is
`gateway-native`, `logged_in`, `logged_out`, or `unknown`. Token and URL
values are never reported.

## Provider scoping (D-021)

Providers created in Remuda are **universal**: Hub stores the encrypted
token and every Node receives the profile through SecretBroker at launch.
A remote host can also have **host-scoped** profiles or bind to **native**
(its own CLI login, no Hub key). Hosts page: 自动 / 原生登录 / 指定
provider. New Session follows the host unless you pick 原生 or 网关.
