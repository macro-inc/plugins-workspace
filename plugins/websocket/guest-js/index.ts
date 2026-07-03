// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import { invoke, Channel } from '@tauri-apps/api/core'

export interface ConnectionConfig {
  /**
   * Read buffer capacity. The default value is 128 KiB.
   */
  readBufferSize?: number
  /** The target minimum size of the write buffer to reach before writing the data to the underlying stream. The default value is 128 KiB.
   *
   * If set to 0 each message will be eagerly written to the underlying stream. It is often more optimal to allow them to buffer a little, hence the default value.
   */
  writeBufferSize?: number
  /** The max size of the write buffer in bytes. Setting this can provide backpressure in the case the write buffer is filling up due to write errors. The default value is unlimited.
   *
   * Note: The write buffer only builds up past write_buffer_size when writes to the underlying stream are failing. So the write buffer can not fill up if you are not observing write errors.
   *
   * Note: Should always be at least write_buffer_size + 1 message and probably a little more depending on error handling strategy.
   */
  maxWriteBufferSize?: number
  /**
   * The maximum size of an incoming message. The string "none" means no size limit. The default value is 64 MiB which should be reasonably big for all normal use-cases but small enough to prevent memory eating by a malicious user.
   */
  maxMessageSize?: number | 'none'
  /**
   * The maximum size of a single incoming message frame. The string "none" means no size limit. The limit is for frame payload NOT including the frame header. The default value is 16 MiB which should be reasonably big for all normal use-cases but small enough to prevent memory eating by a malicious user.
   */
  maxFrameSize?: number | 'none'
  /**
   * When set to true, the server will accept and handle unmasked frames from the client. According to the RFC 6455, the server must close the connection to the client in such cases, however it seems like there are some popular libraries that are sending unmasked frames, ignoring the RFC. By default this option is set to false, i.e. according to RFC 6455.
   */
  acceptUnmaskedFrames?: boolean
  /**
   * Additional connect request headers.
   */
  headers?: HeadersInit
  /**
   * Number of incoming messages retained on the Rust side for replay via
   * {@link WebSocket.recover}. Defaults to 256. Set to 0 to disable buffering.
   */
  replayBufferSize?: number
}

export interface MessageKind<T, D> {
  type: T
  data: D
}

export interface CloseFrame {
  code: number
  reason: string
}

export type Message =
  | MessageKind<'Text', string>
  | MessageKind<'Binary', number[]>
  | MessageKind<'Ping', number[]>
  | MessageKind<'Pong', number[]>
  | MessageKind<'Close', CloseFrame | null>

interface ReplayEnvelope {
  seq: number
  message: Message
}

interface RecoverResponse {
  messages: ReplayEnvelope[]
  gap: boolean
}

interface SocketState {
  lastSeq: number
  recovering: boolean
  queued: ReplayEnvelope[]
}

export default class WebSocket {
  id: number
  private readonly listeners: Set<(arg: Message) => void>
  private readonly state: SocketState
  private readonly deliver: (envelope: ReplayEnvelope) => void

  constructor(
    id: number,
    listeners: Set<(arg: Message) => void>,
    state: SocketState,
    deliver: (envelope: ReplayEnvelope) => void
  ) {
    this.id = id
    this.listeners = listeners
    this.state = state
    this.deliver = deliver
  }

  static async connect(
    url: string,
    config?: ConnectionConfig
  ): Promise<WebSocket> {
    const listeners: Set<(arg: Message) => void> = new Set()
    const state: SocketState = { lastSeq: 0, recovering: false, queued: [] }

    const deliver = (envelope: ReplayEnvelope): void => {
      if (envelope.seq > 0) {
        // deduplicate: replay and live delivery may overlap
        if (envelope.seq <= state.lastSeq) {
          return
        }
        state.lastSeq = envelope.seq
      }
      listeners.forEach((l) => {
        l(envelope.message)
      })
    }

    const onMessage = new Channel<ReplayEnvelope>()
    onMessage.onmessage = (envelope: ReplayEnvelope): void => {
      if (state.recovering) {
        state.queued.push(envelope)
      } else {
        deliver(envelope)
      }
    }

    if (config?.headers) {
      config.headers = Array.from(new Headers(config.headers).entries())
    }

    return await invoke<number>('plugin:websocket|connect', {
      url,
      onMessage,
      config
    }).then((id) => new WebSocket(id, listeners, state, deliver))
  }

  /**
   * Requests redelivery of messages that were received by the Rust side but
   * may not have reached this webview, e.g. while it was suspended in the
   * background. Recovered messages are dispatched to the registered listeners
   * in order; messages that were already delivered are skipped.
   *
   * Call this when the document becomes visible again
   * (`document.visibilitychange`).
   *
   * @returns `true` if messages were missed but already evicted from the
   * replay buffer, meaning the application must resync by other means.
   */
  async recover(): Promise<boolean> {
    if (this.state.recovering) {
      return false
    }
    this.state.recovering = true
    try {
      const res = await invoke<RecoverResponse>('plugin:websocket|recover', {
        id: this.id,
        after: this.state.lastSeq
      })
      for (const envelope of res.messages) {
        this.deliver(envelope)
      }
      return res.gap
    } finally {
      // flush messages that arrived live while we were recovering
      const queued = this.state.queued
      this.state.queued = []
      this.state.recovering = false
      for (const envelope of queued) {
        this.deliver(envelope)
      }
    }
  }

  addListener(cb: (arg: Message) => void): () => void {
    this.listeners.add(cb)

    return () => {
      this.listeners.delete(cb)
    }
  }

  async send(message: Message | string | number[]): Promise<void> {
    let m: Message
    if (typeof message === 'string') {
      m = { type: 'Text', data: message }
    } else if (typeof message === 'object' && 'type' in message) {
      m = message
    } else if (Array.isArray(message)) {
      m = { type: 'Binary', data: message }
    } else {
      throw new Error(
        'invalid `message` type, expected a `{ type: string, data: any }` object, a string or a numeric array'
      )
    }
    await invoke('plugin:websocket|send', {
      id: this.id,
      message: m
    })
  }

  async disconnect(): Promise<void> {
    await this.send({
      type: 'Close',
      data: {
        code: 1000,
        reason: 'Disconnected by client'
      }
    })
  }
}
