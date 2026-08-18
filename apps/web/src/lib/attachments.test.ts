import { describe, expect, test } from 'bun:test'
import type { MessageAttachment, WakuClient } from '@wakuwaku/client'
import { importDaemonPathAttachment, promptInputFromAttachments } from './attachments'

describe('importDaemonPathAttachment', () => {
  test('asks the daemon to import an absolute path without sending file bytes', async () => {
    let command: unknown
    const client = {
      request: async (next: unknown) => {
        command = next
        return {
          type: 'attachmentStored',
          attachment: {
            reference: 'wakuwaku-attachment:one',
            path: '/home/me/.wakuwaku/attachments/one/logo.png',
            name: 'logo.png',
            isDir: false,
          },
        }
      },
    } as unknown as WakuClient

    const attachment = await importDaemonPathAttachment(client, '/Users/me/Pictures/logo.png')

    expect(command).toEqual({
      type: 'importPathAttachment',
      path: '/Users/me/Pictures/logo.png',
    })
    expect(attachment).toEqual({
      path: '/home/me/.wakuwaku/attachments/one/logo.png',
      mention: '/Users/me/Pictures/logo.png',
      name: 'logo.png',
      is_dir: false,
      is_image: true,
      blob_reference: 'wakuwaku-attachment:one',
    })
  })
})

describe('promptInputFromAttachments', () => {
  const notes: MessageAttachment = {
    path: '/home/me/.wakuwaku/attachments/one/notes.md',
    mention: 'notes.md',
    name: 'notes.md',
    is_dir: false,
    is_image: false,
    blob_reference: 'wakuwaku-attachment:notes',
  }
  const shot: MessageAttachment = {
    path: '/home/me/.wakuwaku/attachments/one/shot.png',
    mention: 'shot.png',
    name: 'shot.png',
    is_dir: false,
    is_image: true,
    blob_reference: 'wakuwaku-blob:shot.png',
  }

  test('keeps display text and source metadata without host paths or base64', () => {
    const input = promptInputFromAttachments('see @notes.md @shot.png', [notes, shot], 'see')
    expect(input).toEqual({
      text: 'see @notes.md @shot.png',
      displayText: 'see',
      attachments: [{ kind: 'blob', reference: 'wakuwaku-blob:shot.png' }],
      sources: [
        {
          reference: 'wakuwaku-attachment:notes',
          mention: 'notes.md',
          name: 'notes.md',
          isDir: false,
          isImage: false,
          mime: null,
        },
        {
          reference: 'wakuwaku-blob:shot.png',
          mention: 'shot.png',
          name: 'shot.png',
          isDir: false,
          isImage: true,
          mime: 'image/png',
        },
      ],
    })
    expect(JSON.stringify(input)).not.toContain('/home/me/.waku')
    expect(JSON.stringify(input)).not.toContain('base64')
  })

  test('drops host-path and data-url image refs from provider attachments', () => {
    const input = promptInputFromAttachments('photo', [{
      ...shot,
      blob_reference: 'data:image/png;base64,aaaa',
    }, {
      ...shot,
      blob_reference: '/tmp/shot.png',
    }])
    expect(input.attachments).toBeUndefined()
    expect(input.sources).toEqual([
      expect.objectContaining({ reference: null, mention: 'shot.png', mime: 'image/png' }),
      expect.objectContaining({ reference: null, mention: 'shot.png', mime: 'image/png' }),
    ])
  })

  test('omits display text when it matches provider text', () => {
    const input = promptInputFromAttachments('hello', [], 'hello')
    expect(input).toEqual({ text: 'hello' })
  })
})
