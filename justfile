# List all recipes
default:
    @just --list

# Run all tests
[group: 'test']
test:
	cargo test --all

# See what linter errors and warnings are unaddressed
[group: 'style']
lint:
	cargo clippy --all

# Run Rustfmt against all source files
[group: 'style']
fmt:
	cargo fmt --all

# Build the project for the current system architecture
# (Gets stored at ./target/[debug|release]/coyote)
[group: 'build']
[arg('build_type', pattern="debug|release")]
build build_type='debug':
	@cargo build {{ if build_type == "release" { "--release" } else { "" } }}

# Build a multi-platform Docker image (linux/amd64 + linux/arm64).
# Requires an active buildx builder with multi-platform support and a registry login.
# version: must match an existing GitHub release tag (e.g. 0.7.4)
# image:   registry/image name to push to (default: darkalex17/coyote)
[group: 'build']
docker-build version image='darkalex17/coyote':
	docker buildx build \
		--platform linux/amd64,linux/arm64 \
		--build-arg COYOTE_VERSION={{ version }} \
		--tag {{ image }}:{{ version }} \
		--tag {{ image }}:latest \
		.
