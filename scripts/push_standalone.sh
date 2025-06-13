#!/bin/bash
# Push profile_aggregator Docker image (standalone repository version)

set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

TAG=${1:-latest}  # Use first argument if provided, otherwise default to "latest"

# Check if image exists locally
if ! docker image inspect ghcr.io/verse-pbc/profile_aggregator:$TAG &> /dev/null; then
    echo -e "${RED}Error: Image ghcr.io/verse-pbc/profile_aggregator:$TAG not found locally${NC}"
    echo -e "${YELLOW}Run ./scripts/build_standalone.sh first${NC}"
    exit 1
fi

# Check environment variables
if [ -z "$GITHUB_USER" ] || [ -z "$GITHUB_API_TOKEN" ]; then
    echo -e "${RED}Error: GITHUB_USER and GITHUB_API_TOKEN environment variables must be set${NC}"
    echo -e "${YELLOW}Example:${NC}"
    echo "  export GITHUB_USER=your-github-username"
    echo "  export GITHUB_API_TOKEN=your-personal-access-token"
    exit 1
fi

# Login to GitHub Container Registry
echo -e "${YELLOW}Logging in to GitHub Container Registry...${NC}"
echo "$GITHUB_API_TOKEN" | docker login ghcr.io -u "$GITHUB_USER" --password-stdin

if [ $? -ne 0 ]; then
    echo -e "${RED}Failed to login to GitHub Container Registry${NC}"
    exit 1
fi

echo -e "${YELLOW}Pushing ghcr.io/verse-pbc/profile_aggregator:$TAG...${NC}"

# Push the image
docker push ghcr.io/verse-pbc/profile_aggregator:$TAG

if [ $? -eq 0 ]; then
    echo -e "${GREEN}Successfully pushed ghcr.io/verse-pbc/profile_aggregator:$TAG${NC}"
else
    echo -e "${RED}Failed to push image${NC}"
    exit 1
fi