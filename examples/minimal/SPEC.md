# Service: Minimal Example

A minimal specification used to start the nosrc HTTP process while the handler runtime is being completed.

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /hello`
- Rules: Return a JSON greeting.
- Response `200`: `{ "message": "hello" }`
