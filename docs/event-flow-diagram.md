# Profile Aggregator Event Flow

## Architecture Overview

```mermaid
graph TD
    subgraph "External Sources"
        ER[External Nostr Relays]
        NC[Nostr Clients]
    end
    
    subgraph "Profile Aggregator Relay"
        subgraph "Aggregation Service"
            AS[Service Manager]
            H[Harvesters<br/>per relay]
            VP[Validation Pool]
            W[Workers]
        end
        
        subgraph "WebSocket Server"
            WS[WebSocket Handler<br/>:8080]
            PQF[Profile Quality Filter]
        end
    end
    
    subgraph "Storage"
        DB[(Shared Database)]
    end
    
    ER -->|websocket| H
    AS -->|manages| H
    H -->|historical events| VP
    H -->|real-time events| VP
    VP -->|queues| W
    W -->|validated profiles| DB
    W -->|metrics| AS
    
    NC -->|REQ/EVENT| WS
    WS -->|EVENT| PQF
    PQF -->|validated| DB
    DB -->|EVENT| WS
    WS -->|EVENT| NC
```

## How It Works

The Profile Aggregator operates as both a harvester AND a Nostr relay:

### 1. **Aggregation Service** (harvests from external relays)
- **Harvesters** connect to each relay and run two parallel streams:
  - Historical: Fetches old events backward in time
  - Real-time: Monitors new events as they arrive
- **Validation Pool** queues events for processing:
  - Immediate queue for new events
  - Delayed queue for rate-limited retries
- **Workers** validate profiles (quality, images, spam) and store results

### 2. **WebSocket Server** (acts as a Nostr relay)
- Listens on port 8080 for WebSocket connections
- Accepts EVENT messages from clients (new profiles)
- Runs incoming events through the same ProfileQualityFilter
- Stores validated events in the shared database
- Responds to REQ subscriptions with filtered profiles
- Implements NIP-01 (basic protocol) and NIP-11 (relay information)

### 3. **Shared Components**
- Both services use the same ProfileQualityFilter
- Both services write to the same database
- Metrics logged every 20 seconds show accepted/rejected/failed counts