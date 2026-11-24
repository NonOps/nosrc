# Service: corpus-created-resource

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `POST /items`
- Rules: Create an item with the submitted name, return its identifier and a Location header, and allow the created item to be retrieved.
- Response `201`: Created with the item identifier.

### `GET /items/:id`
- Rules: Return the created item, including its name.
- Response `200`: JSON object containing the item name.

## Verification
### `POST /items`
- Body: {"name":"widget"}
- Response `201`
- Body contains: "name":"widget"
- Response header: location=/items/1

### `GET /items/1`
- Response `200`
- Body contains: "name":"widget"
