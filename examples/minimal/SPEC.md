# Service: Minimal Example

A minimal service specification for the nosrc JavaScript runtime.

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /hello`
- Rules: Return a JSON greeting.
- Response `200`: `{ "message": "hello" }`
