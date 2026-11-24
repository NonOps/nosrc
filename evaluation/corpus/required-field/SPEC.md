# Service: corpus-required-field

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `POST /users`
- Rules: Require a non-empty name.
- Response `201`: User accepted.
- Response `400`: Name is required.

## Verification
### `POST /users`
- Body: {}
- Response `400`
- Body contains: name is required

### `POST /users`
- Body: {"name":"Ada"}
- Response `201`
- Body contains: created
