# Service: corpus-plain-text

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /healthz`
- Rules: Return a plain-text health response.
- Response `200`: Healthy.

## Verification
### `GET /healthz`
- Response `200`
- Body contains: healthy
- Response header: content-type=text/plain
