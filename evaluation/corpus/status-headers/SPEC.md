# Service: corpus-status-headers

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /jobs/queued`
- Rules: Return the queued job state and a response header.
- Response `202`: Queued.

## Verification
### `GET /jobs/queued`
- Response `202`
- Body contains: queued
- Response header: x-job-state=queued
