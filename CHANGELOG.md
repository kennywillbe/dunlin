# Changelog

## [0.3.2](https://github.com/kennywillbe/dunlin/compare/v0.3.1...v0.3.2) (2026-10-30)


### Bug Fixes

* **deps:** bump bollard from 0.18.1 to 0.21.1 ([#33](https://github.com/kennywillbe/dunlin/issues/33)) ([af6538d](https://github.com/kennywillbe/dunlin/commit/af6538d419c595a728bcb821e08b28e97bc22c51))
* **deps:** bump nix from 0.30.1 to 0.31.3 ([#24](https://github.com/kennywillbe/dunlin/issues/24)) ([088b476](https://github.com/kennywillbe/dunlin/commit/088b476343e57b0a3db66796a6e11ceac237af85))
* **deps:** bump the patch-updates group across 1 directory with 4 updates ([#32](https://github.com/kennywillbe/dunlin/issues/32)) ([4dc163c](https://github.com/kennywillbe/dunlin/commit/4dc163ccb6d68dc584e6f66e111310ef958564c1))
* **deps:** bump toml from 0.8.23 to 1.1.6+spec-1.1.0 ([#22](https://github.com/kennywillbe/dunlin/issues/22)) ([8cba039](https://github.com/kennywillbe/dunlin/commit/8cba0393443d6defd5d355ecbbfce114e13c8027))
* **deps:** bump x509-parser from 0.16.0 to 0.18.1 ([#23](https://github.com/kennywillbe/dunlin/issues/23)) ([3fc8793](https://github.com/kennywillbe/dunlin/commit/3fc87931f3b56f3e451ff1b994979f56aede7624))
* **deps:** clear the clippy and audit failures on current stable ([#31](https://github.com/kennywillbe/dunlin/issues/31)) ([409034c](https://github.com/kennywillbe/dunlin/commit/409034c67e66ec65a9b0ae000c5b4770e8ed5c94))


### Dependencies

* **deps:** bump taiki-e/install-action from 2.87.17 to 2.87.32 ([#25](https://github.com/kennywillbe/dunlin/issues/25)) ([7bd65cc](https://github.com/kennywillbe/dunlin/commit/7bd65cc6b136721d208ff3e239f08f12e1db0e8f))

## [0.3.1](https://github.com/kennywillbe/dunlin/compare/v0.3.0...v0.3.1) (2026-09-26)


### Performance

* read the metrics picker from a series table, not a scan of samples ([#18](https://github.com/kennywillbe/dunlin/issues/18)) ([ea4913f](https://github.com/kennywillbe/dunlin/commit/ea4913f34717ad6cf3e34f4b5b6f11ef04978a45))

## [0.3.0](https://github.com/kennywillbe/dunlin/compare/v0.2.0...v0.3.0) (2026-09-24)


### Features

* add notifier channels, badges, Prometheus, API and subscriptions ([#16](https://github.com/kennywillbe/dunlin/issues/16)) ([df1d935](https://github.com/kennywillbe/dunlin/commit/df1d935b8ad08b2f353b47a61ed9f36b5e10134d))

## [0.2.0](https://github.com/kennywillbe/dunlin/compare/v0.1.0...v0.2.0) (2026-09-24)


### Features

* add public_url and fix issues found reviewing 0.1.0 ([#13](https://github.com/kennywillbe/dunlin/issues/13)) ([6b0f457](https://github.com/kennywillbe/dunlin/commit/6b0f457438bfc801ea48f029f4b562d7f04a0ad5))


### Bug Fixes

* close incidents of removed checks and reload once per save ([#12](https://github.com/kennywillbe/dunlin/issues/12)) ([d3cd072](https://github.com/kennywillbe/dunlin/commit/d3cd0722088cb18fe5a890a8f9a76b8db52ff58f))

## 0.1.0 (2026-09-23)

First public version. HTTP, TCP, systemd, Docker, resource and heartbeat
checks, incidents with a timeline, maintenance windows, alerts to Telegram or
a webhook, a daily summary, and a status page with 90 days of history.


### Features

* status page built around a quiet line ([f5f306d](https://github.com/kennywillbe/dunlin/commit/f5f306d81063fe0f5825736bca47ddef30b66a68))


### Bug Fixes

* count days in the configured timezone ([#9](https://github.com/kennywillbe/dunlin/issues/9)) ([e42d2ff](https://github.com/kennywillbe/dunlin/commit/e42d2ffd8f61eba2328c9872ac413fd599b6be88))
* **deps:** bump argon2, rand, sha2 and quick-xml ([2b47fa8](https://github.com/kennywillbe/dunlin/commit/2b47fa8fbf11d791878713856b19aa97f59331bf))
* **deps:** bump sqlx to 0.9 and the MSRV to 1.94 ([90bc65b](https://github.com/kennywillbe/dunlin/commit/90bc65b367b5eb9f5fd73071afcf410f264930a2))
* stop the page from stretching on wide screens ([b4c57b7](https://github.com/kennywillbe/dunlin/commit/b4c57b7da81ab0e9b159cc1e4d3e9f9105c3072b))


### Dependencies

* **deps:** bump docker/setup-qemu-action from 4.2.0 to 4.4.0 ([#7](https://github.com/kennywillbe/dunlin/issues/7)) ([da98a20](https://github.com/kennywillbe/dunlin/commit/da98a204169547d5c0f7c24f4bbcbf7a28e6b1e0))
* **deps:** bump rust from 1.88-bookworm to 1.98-bookworm ([#1](https://github.com/kennywillbe/dunlin/issues/1)) ([2cc7c3b](https://github.com/kennywillbe/dunlin/commit/2cc7c3b89c82b25bd41b0e8d5def8d2acdf9cbe4))
* **deps:** bump taiki-e/install-action from 2.87.1 to 2.87.17 ([#2](https://github.com/kennywillbe/dunlin/issues/2)) ([5767725](https://github.com/kennywillbe/dunlin/commit/57677250664bae9ab034b41c1daf330a0c536fab))
