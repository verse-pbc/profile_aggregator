# Profile Aggregator Pagination Gap Analysis

## Overview

The profile aggregator uses a dual-pagination approach to collect profile events from Nostr relays:
1. **Real-time subscription**: Monitors new events going forward from connection time
2. **Backward pagination**: Fetches historical events going backward in time

This document analyzes a critical issue where gaps in event collection can occur during service restarts.

## Current Implementation

### State Persistence

The service persists its state to `./data/aggregation_state.json`:

```json
{
  "relay_states": {
    "wss://relay.nos.social": {
      "window_start": 1749975897,
      "last_until_timestamp": 1733190044,
      "total_events_fetched": 8279143,
      "window_complete": true
    }
  },
  "last_saved": 1749975895
}
```

### Pagination Behavior

1. **On initial start**:
   - Real-time subscription starts from `Timestamp::now()`
   - Backward pagination starts from `Timestamp::now()` going backward

2. **On restart**:
   - Real-time subscription starts fresh from `Timestamp::now()`
   - Backward pagination resumes from saved `last_until_timestamp`

3. **Window completion**:
   - After 3 consecutive empty pages, backward pagination marks window as complete
   - Next window starts from current time, potentially creating permanent gaps

## The Gap Problem

### Scenario Example

Consider this timeline of events:

```
Time:    0 -------- 9 -- 10 -- 11 -------------------- 20 ----> future
                    ↑     ↑     ↑                      ↑
                    |     |     |                      |
                    |     |     Real-time reaches      Service
                    |     |     this point             restarts
                    |     |                            
                    |     Service starts:              
                    |     - Backward from 10           
                    |     - Real-time from 10          
                    |                                  
                    Backward pagination                
                    reaches here when                  
                    service stops                      
```

**Result**: Events between timestamps 11-20 are never collected.

### Why Gaps Occur

1. **No boundary tracking**: The backward pagination doesn't know where real-time subscription ended
2. **Fresh real-time start**: Real-time always starts from "now" on restart, not from where it left off
3. **No gap detection**: There's no logic to identify missing time ranges
4. **No overlap prevention**: Backward pagination can process events already seen by real-time

## Impact

### Data Loss Scenarios

1. **Service downtime**: Any events published while service is down
2. **Restart gaps**: Events between last real-time position and restart time
3. **Window resets**: When window completes, the gap between old window and new window start

### Duplicate Processing

Since there's no coordination between real-time and backward pagination:
- Events can be processed twice if backward pagination reaches timestamps already covered by real-time
- No deduplication mechanism exists at the pagination level

## Missing Components

The current implementation lacks:

1. **Real-time position tracking**: No persistence of the last real-time event timestamp
2. **Gap detection logic**: No mechanism to identify time ranges with missing events
3. **Boundary enforcement**: Backward pagination doesn't stop at real-time subscription start
4. **Gap filling strategy**: No targeted queries to fetch events from identified gaps

## Code References

Key locations in the codebase:

- State persistence: `profile_aggregation_service.rs:186-222`
- Backward pagination: `profile_aggregation_service.rs:337-484`
- Real-time subscription: `profile_aggregation_service.rs:487-592`
- Window completion logic: `profile_aggregation_service.rs:467-479`

## Next Steps

To address these issues, the service needs:

1. Track real-time subscription boundaries
2. Implement gap detection between pagination endpoints
3. Add targeted gap-filling queries
4. Prevent duplicate processing with boundary checks
5. Consider a more robust event tracking mechanism

---

*Document created: 2025-01-15*