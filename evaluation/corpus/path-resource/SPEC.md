# Service: corpus-path-resource

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /items/:id`
- Rules: Return the requested item identifier.
- Response `200`: JSON object containing id.

## Verification
### `GET /items/42`
- Response `200`
- Body contains: "id":"42"
