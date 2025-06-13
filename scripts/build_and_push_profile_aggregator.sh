#!/bin/bash
# Build and push profile_aggregator Docker image (convenience script)

set -e

# This script combines build and push for convenience
# For individual operations, use:
#   ./scripts/build_profile_aggregator.sh
#   ./scripts/push_profile_aggregator.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build the image
"$SCRIPT_DIR/build_profile_aggregator.sh"

# Push the image
"$SCRIPT_DIR/push_profile_aggregator.sh"