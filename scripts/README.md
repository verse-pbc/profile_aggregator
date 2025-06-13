# Profile Aggregator Scripts

This directory contains scripts for building and deploying the profile_aggregator Docker image.

## Current Workspace Scripts

These scripts work when profile_aggregator is part of the groups_relay workspace:

- `build_profile_aggregator.sh` - Builds the Docker image for linux/amd64
- `push_profile_aggregator.sh` - Pushes the image to GitHub Container Registry
- `build_and_push_profile_aggregator.sh` - Convenience script that builds and pushes
- `tag_profile_aggregator_as_stable.sh` - Tags an image as stable for deployment

## Standalone Repository Scripts

These scripts will work when profile_aggregator is moved to its own repository:

- `build_standalone.sh` - Builds the Docker image when in standalone repo
- `push_standalone.sh` - Pushes the image when in standalone repo

## Usage

### In Current Workspace

```bash
# From anywhere in the workspace
cd crates/profile_aggregator

# Build the image
./scripts/build_profile_aggregator.sh

# Set credentials
export GITHUB_USER=your-username
export GITHUB_API_TOKEN=your-token

# Push the image
./scripts/push_profile_aggregator.sh

# Or do both
./scripts/build_and_push_profile_aggregator.sh

# Tag as stable for deployment
./scripts/tag_profile_aggregator_as_stable.sh
```

### When Moved to Standalone Repository

```bash
# Build the image
./scripts/build_standalone.sh

# Push the image
./scripts/push_standalone.sh
```

## Environment Variables

- `GITHUB_USER` - Your GitHub username
- `GITHUB_API_TOKEN` - GitHub Personal Access Token with `write:packages` permission

## Image Tags

- `latest` - Latest build from main branch
- `stable` - Production-ready version
- Custom tags can be specified as arguments to push scripts