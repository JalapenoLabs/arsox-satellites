// Copyright © 2026 Jalapeno Labs

import type { ThreadSettingsInit } from './satellite.js'
import type { McpServer, Service } from './index.js'

// Core
import { describe, expect, expectTypeOf, it } from 'vitest'
import { fromBinary } from '@bufbuild/protobuf'

// Misc
import { ServiceIsolation } from './index.js'
import { ThreadSettingsSchema } from './proto/arsox/settings/v1/settings_pb.js'
import { encode } from './transport.js'

// The shape a host application sends for a helper process and the MCP server
// that talks to it: the server is addressed through the service, because its
// port only exists once a turn has started the service.
const declared: ThreadSettingsInit = {
  idleTtl: { seconds: 3600n },
  budget: {},
  services: [
    {
      name: 'blender-mcp',
      command: 'blender --background --python bridge.py -- --port "$PORT"',
      readyWhen: { httpGet: '/health', timeout: { seconds: 120n } },
      isolation: ServiceIsolation.SHARED
    }
  ],
  mcpServers: [
    {
      name: 'blender',
      service: { service: 'blender-mcp', path: '/mcp' }
    }
  ]
}

describe('ThreadSettings services', () => {
  it('carries a thread service and an MCP server it runs across the wire', () => {
    const decoded = fromBinary(ThreadSettingsSchema, encode(ThreadSettingsSchema, declared))

    const [ service ] = decoded.services
    expect(decoded.services).toHaveLength(1)
    expect(service?.name).toBe('blender-mcp')
    expect(service?.readyWhen?.httpGet).toBe('/health')
    expect(service?.isolation).toBe(ServiceIsolation.SHARED)

    const [ server ] = decoded.mcpServers
    expect(server?.service?.service).toBe('blender-mcp')
    expect(server?.service?.path).toBe('/mcp')
    // Exactly one of the two addresses: a server reached through a service
    // leaves its url empty.
    expect(server?.url).toBe('')
  })

  it('leaves an undeclared port absent so the satellite assigns one', () => {
    // Absent is not zero. A port of 0 would be a declared port the satellite
    // refuses, not a request for one to be chosen.
    const decoded = fromBinary(ThreadSettingsSchema, encode(ThreadSettingsSchema, declared))

    expect(decoded.services[0]?.port).toBeUndefined()
  })

  it('exposes the service messages as types a consumer can name', () => {
    const decoded = fromBinary(ThreadSettingsSchema, encode(ThreadSettingsSchema, declared))

    expectTypeOf(decoded.services).toEqualTypeOf<Service[]>()
    expectTypeOf(decoded.mcpServers).toEqualTypeOf<McpServer[]>()
  })
})
