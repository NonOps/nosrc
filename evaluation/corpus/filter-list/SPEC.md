# Service: corpus-filter-list

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `POST /active`
- Rules: Return only active records from the submitted JSON array.
- Response `200`: JSON array of active records.

## Verification
### `POST /active`
- Body: [{"name":"alpha","active":true},{"name":"beta","active":false}]
- Response `200`
- Body contains: [{"name":"alpha","active":true}]
