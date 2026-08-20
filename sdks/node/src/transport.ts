// Copyright © 2026 Jalapeno Labs

import type { Error as ContractError } from './proto/arsox/error/v1/error_pb.js'
import type { DescMessage, MessageInitShape, MessageShape } from '@bufbuild/protobuf'

// Core
import { request as httpRequest } from 'node:http'
import { request as httpsRequest } from 'node:https'
import { create, fromBinary, toBinary } from '@bufbuild/protobuf'

// Misc
import { ErrorSchema } from './proto/arsox/error/v1/error_pb.js'
import { ArsoxError } from './error.js'
import { PROTOBUF_CONTENT_TYPE } from './constants.js'

/** The methods the satellite's HTTP surface uses. */
export type HttpMethod = 'GET' | 'POST' | 'DELETE'

/** What came back off the socket, before anything is made of it. */
export type RawResponse = {
  status: number
  body: Uint8Array
}

/**
 * Encodes a message for the wire.
 *
 * `create` then `toBinary` is the protobuf-es pair, and writing it out at every
 * call site means naming the schema twice for one message, which is exactly the
 * kind of duplication that eventually names two different schemas.
 */
export function encode<Desc extends DescMessage>(
  schema: Desc,
  init: MessageInitShape<Desc>
): Uint8Array {
  return toBinary(schema, create(schema, init))
}

/**
 * Turns a raw response into the message it carries, or into the failure it
 * describes.
 *
 * Separated from the socket work so the mapping from a status and some bytes to
 * an {@link ArsoxError} is testable without a satellite, a port, or a mock.
 */
export function decodeResponse<Desc extends DescMessage>(
  schema: Desc,
  response: RawResponse
): MessageShape<Desc> {
  if (response.status >= 200 && response.status < 300) {
    try {
      return fromBinary(schema, response.body)
    }
    catch (error) {
      throw ArsoxError.transport(
        `the satellite sent an undecodable response: ${String(error)}`
      )
    }
  }

  // Every failure on every transport is the same shape, so an empty or
  // undecodable error body means something other than a satellite answered.
  if (response.body.byteLength === 0) {
    throw ArsoxError.transport(`the satellite answered ${response.status} with no body`)
  }

  let contractError: ContractError
  try {
    contractError = fromBinary(ErrorSchema, response.body)
  }
  catch {
    throw ArsoxError.transport(`the satellite answered ${response.status}`)
  }

  throw ArsoxError.contract(contractError)
}

/**
 * One satellite's address and credential.
 *
 * Handles hold one of these and nothing else, which is what makes a handle
 * disposable and a thread durable.
 */
export class Connection {
  /** The satellite's base URL, without a trailing slash. */
  readonly baseUrl: string

  readonly secret: string

  constructor(url: string, secret: string) {
    this.baseUrl = url.replace(/\/+$/u, '')
    this.secret = secret
  }

  /** The same satellite, addressed as a WebSocket. */
  get socketUrl(): string {
    if (this.baseUrl.startsWith('https://')) {
      return `wss://${this.baseUrl.slice('https://'.length)}`
    }
    if (this.baseUrl.startsWith('http://')) {
      return `ws://${this.baseUrl.slice('http://'.length)}`
    }
    return this.baseUrl
  }

  /**
   * Sends one request and decodes what comes back.
   *
   * `body` is optional because several endpoints take none, and present on GET
   * for the listings, whose filters are a protobuf request message like every
   * other request in the contract.
   */
  async call<Desc extends DescMessage>(
    method: HttpMethod,
    path: string,
    schema: Desc,
    body?: Uint8Array
  ): Promise<MessageShape<Desc>> {
    return decodeResponse(schema, await this.transmit(method, path, body))
  }

  /**
   * Puts one request on the wire.
   *
   * Built on `node:http` rather than `fetch`. The contract carries filters for
   * the listing endpoints in a protobuf request body on GET, and the WHATWG
   * fetch specification forbids a body on GET outright, so `fetch` throws before
   * a byte leaves the process. This is not a preference for the older API.
   */
  private transmit(
    method: HttpMethod,
    path: string,
    body: Uint8Array | undefined
  ): Promise<RawResponse> {
    const url = new URL(this.baseUrl + path)
    const send = url.protocol === 'https:'
      ? httpsRequest
      : httpRequest

    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.secret}`,
      Accept: PROTOBUF_CONTENT_TYPE
    }
    if (body) {
      headers['Content-Type'] = PROTOBUF_CONTENT_TYPE
      headers['Content-Length'] = String(body.byteLength)
    }

    return new Promise((resolve, reject) => {
      const outgoing = send(url, { method, headers }, (incoming) => {
        const chunks: Buffer[] = []

        incoming.on('data', (chunk: Buffer) => chunks.push(chunk))
        incoming.on('error', (error: Error) => reject(ArsoxError.transport(error.message)))
        incoming.on('end', () => resolve({
          status: incoming.statusCode ?? 0,
          body: Buffer.concat(chunks)
        }))
      })

      outgoing.on('error', (error: Error) => reject(ArsoxError.transport(error.message)))

      if (body) {
        outgoing.write(body)
      }
      outgoing.end()
    })
  }
}
