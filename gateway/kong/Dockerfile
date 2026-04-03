FROM kong:3.9.1

USER root

RUN mkdir -p /usr/local/share/lua/5.1/kong/plugins/solana-kong-402
COPY solana-pay/gateway/kong/kong/plugins/solana-kong-402 /usr/local/share/lua/5.1/kong/plugins/solana-kong-402
COPY solana-pay/gateway/kong/mpp /usr/local/share/lua/5.1/mpp

COPY solana-pay/gateway/kong/dev/kong.conf /etc/kong/kong.conf
COPY solana-pay/gateway/kong/dev/kong.yml /etc/kong/kong.yml

USER kong
