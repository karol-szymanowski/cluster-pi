.PHONY: all build build-arm64 test test-chaos test-netns lint fmt check docker-build k3s-deploy helm-install helm-template helm-lint clean

CARGO ?= cargo
TARGET_AARCH64 = aarch64-unknown-linux-musl
TARGET_X86_64 = x86_64-unknown-linux-musl
HELM ?= helm

all: lint test build

build:
	$(CARGO) build --workspace

build-arm64:
	$(CARGO) build --workspace --target $(TARGET_AARCH64) --release

test:
	$(CARGO) test --workspace

test-chaos:
	$(CARGO) test -p chaos-tests -- --nocapture

# Network namespace tests may require CAP_NET_ADMIN / sudo when running against real Linux network namespaces
test-netns:
	$(CARGO) test -p netns-tests -- --nocapture

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) fmt --check

fmt:
	$(CARGO) fmt

check:
	$(CARGO) check --workspace --all-targets

docker-build:
	docker build -f deploy/docker/Dockerfile.netboot -t cluster-netboot:latest .
	docker build -f deploy/docker/Dockerfile.operator -t cluster-operator:latest .

helm-lint:
	$(HELM) lint deploy/helm/pi-cluster-core

helm-template:
	$(HELM) template pi-cluster-core deploy/helm/pi-cluster-core -n kube-system

helm-install:
	$(HELM) upgrade --install pi-cluster-core deploy/helm/pi-cluster-core -n kube-system --create-namespace

k3s-deploy: helm-install

clean:
	$(CARGO) clean
