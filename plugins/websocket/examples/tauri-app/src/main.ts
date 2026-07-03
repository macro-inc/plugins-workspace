// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import WebSocket from 'tauri-plugin-websocket-api'
import './style.css'

let ws: WebSocket

document.addEventListener('DOMContentLoaded', async () => {
  document.querySelector('#send')?.addEventListener('click', send)
  document.querySelector('#disconnect')?.addEventListener('click', disconnect)
  await connect()
})

// Recover messages that were received by the Rust side while the webview was
// suspended/hidden (e.g. iOS backgrounding).
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible' || !ws) {
    return
  }
  ws.recover()
    .then((gap) => {
      if (gap) {
        // messages were missed but already evicted from the replay buffer;
        // the application must resync through its own protocol
        _updateResponse('Message gap detected, resync required')
      }
    })
    .catch(_updateResponse)
})

function _updateResponse(returnValue: unknown) {
  const msg = document.createElement('p')
  msg.textContent =
    typeof returnValue === 'string' ? returnValue : JSON.stringify(returnValue)
  document.querySelector('#response-container')?.appendChild(msg)
}

async function connect() {
  try {
    ws = await WebSocket.connect('ws://127.0.0.1:8080').then((r) => {
      _updateResponse('Connected')
      return r
    })
  } catch (e) {
    _updateResponse(e)
  }
  ws.addListener(_updateResponse)
}

function send() {
  ws.send(document.querySelector('#msg-input')?.textContent || '')
    .then(() => {
      _updateResponse('Message sent')
    })
    .catch(_updateResponse)
}

function disconnect() {
  ws.disconnect()
    .then(() => {
      _updateResponse('Disconnected')
    })
    .catch(_updateResponse)
}

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
  <div>
    <input type="text" />
    <button id="send">send</button>
    <button id="disconnect">disconnect</button>
    <div id="response-container"></div>
  </div>
`
