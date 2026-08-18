import {
  MAX_WIRE_MESSAGE_BYTES,
  type MessageAttachment,
  type PromptAttachmentSource,
  type PromptImageRef,
  type PromptInput,
  type WakuClient,
} from '@wakuwaku/client'

const MAX_UPLOAD_BYTES = Math.floor((MAX_WIRE_MESSAGE_BYTES * 3) / 4) - 1024 * 1024

export async function importFiles(
  client: WakuClient,
  files: File[],
): Promise<MessageAttachment[]> {
  const attachments: MessageAttachment[] = []
  for (const file of files) {
    if (file.size > MAX_UPLOAD_BYTES) {
      throw new Error(`${file.name} is too large to send to the daemon`)
    }
    const response = await client.request({
      type: 'importAttachment',
      name: file.name,
      upload: {
        kind: 'file',
        data_base64: await fileBase64(file),
      },
    })
    if (response.type !== 'attachmentStored') {
      throw new Error(`Expected attachmentStored, received ${response.type}`)
    }
    attachments.push({
      path: response.attachment.path,
      mention: response.attachment.path,
      name: response.attachment.name,
      is_dir: response.attachment.isDir,
      is_image: file.type.startsWith('image/') || isImageName(file.name),
      blob_reference: response.attachment.reference,
    })
  }
  return attachments
}

export async function importDaemonPathAttachment(
  client: WakuClient,
  path: string,
): Promise<MessageAttachment> {
  const response = await client.request({
    type: 'importPathAttachment',
    path,
  })
  if (response.type !== 'attachmentStored') {
    throw new Error(`Expected attachmentStored, received ${response.type}`)
  }
  const mention = response.attachment.isDir && !path.endsWith('/') ? `${path}/` : path
  return {
    path: response.attachment.path,
    mention,
    name: response.attachment.name,
    is_dir: response.attachment.isDir,
    is_image: !response.attachment.isDir && isImageName(response.attachment.name),
    blob_reference: response.attachment.reference,
  }
}

export function promptInputFromAttachments(
  text: string,
  attachments: MessageAttachment[],
  displayText?: string,
): PromptInput {
  const sources: PromptAttachmentSource[] = attachments.map((attachment) => {
    const reference = attachment.blob_reference
    const stored = reference?.startsWith('wakuwaku-blob:') || reference?.startsWith('wakuwaku-attachment:')
      ? reference
      : null
    return {
      reference: stored ?? null,
      mention: attachment.mention,
      name: attachment.name,
      isDir: attachment.is_dir,
      isImage: attachment.is_image,
      mime: attachment.is_image ? sourceImageMime(attachment.name) : null,
    }
  })
  const imageAttachments: PromptImageRef[] = attachments.flatMap((attachment) => {
    if (!attachment.is_image) return []
    const reference = attachment.blob_reference
    if (!reference) return []
    if (reference.startsWith('wakuwaku-blob:')) {
      return [{ kind: 'blob' as const, reference }]
    }
    if (reference.startsWith('wakuwaku-attachment:')) {
      return [{ kind: 'attachment' as const, reference }]
    }
    return []
  })
  return {
    text,
    ...(displayText !== undefined && displayText !== text ? { displayText } : {}),
    ...(imageAttachments.length > 0 ? { attachments: imageAttachments } : {}),
    ...(sources.length > 0 ? { sources } : {}),
  }
}

export async function readAttachmentImage(
  client: WakuClient,
  attachment: MessageAttachment,
): Promise<string> {
  const reference = attachment.blob_reference
  if (!reference) throw new Error('This attachment has no daemon reference')
  const command = reference.startsWith('wakuwaku-blob:')
    ? ({ type: 'readBlob', reference } as const)
    : ({ type: 'readAttachment', reference, path: attachment.path } as const)
  const response = await client.request(command)
  if (response.type !== 'blobData') {
    throw new Error(`Expected blobData, received ${response.type}`)
  }
  return `data:${imageMimeType(attachment.name)};base64,${response.bytes}`
}

function imageMimeType(name: string): string {
  const extension = name.split('.').at(-1)?.toLowerCase()
  return (
    {
      avif: 'image/avif',
      gif: 'image/gif',
      heic: 'image/heic',
      jpeg: 'image/jpeg',
      jpg: 'image/jpeg',
      png: 'image/png',
      svg: 'image/svg+xml',
      webp: 'image/webp',
    }[extension ?? ''] ?? 'application/octet-stream'
  )
}

function sourceImageMime(name: string): string | null {
  const mime = imageMimeType(name)
  return mime === 'application/octet-stream' ? null : mime
}

function fileBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(reader.error ?? new Error(`Could not read ${file.name}`))
    reader.onload = () => {
      const result = String(reader.result)
      const separator = result.indexOf(',')
      if (separator === -1) reject(new Error(`Could not encode ${file.name}`))
      else resolve(result.slice(separator + 1))
    }
    reader.readAsDataURL(file)
  })
}

function isImageName(name: string): boolean {
  return ['avif', 'gif', 'heic', 'jpeg', 'jpg', 'png', 'svg', 'webp'].includes(
    name.split('.').at(-1)?.toLowerCase() ?? '',
  )
}
