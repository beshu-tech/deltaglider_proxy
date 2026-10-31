import { isHtmlKey } from './htmlPreview';

const TEXT_EXTENSIONS = new Set([
  'txt', 'md', 'json', 'yaml', 'yml', 'xml', 'csv', 'log',
  'toml', 'ini', 'cfg', 'conf', 'properties', 'env',
  'sh', 'bash', 'py', 'rs', 'js', 'ts', 'css',
  'sha', 'sha1', 'sha256', 'sha512', 'sum',
  'gitignore', 'dockerignore', 'dockerfile', 'makefile',
  'license', 'readme', 'changelog',
]);

const IMAGE_EXTENSIONS = new Set([
  'jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'bmp', 'ico',
]);

// Formats browsers can play natively in a <video> element. Container
// extensions (mp4/webm/ogg/mov) — actual playability still depends on the
// codec inside, but these cover the overwhelming majority of real files.
const VIDEO_EXTENSIONS = new Set([
  'mp4', 'webm', 'ogv', 'mov', 'm4v',
]);

// Formats browsers can play natively in an <audio> element.
const AUDIO_EXTENSIONS = new Set([
  'mp3', 'wav', 'ogg', 'oga', 'm4a', 'aac', 'flac', 'opus',
]);

type PreviewMode = 'html' | 'text' | 'image' | 'video' | 'audio';

export function getPreviewMode(filename: string): PreviewMode | null {
  const ext = filename.split('.').pop()?.toLowerCase() ?? '';
  const basename = filename.split('/').pop()?.toLowerCase() ?? '';
  if (isHtmlKey(filename)) return 'html';
  if (TEXT_EXTENSIONS.has(ext) || TEXT_EXTENSIONS.has(basename)) return 'text';
  if (IMAGE_EXTENSIONS.has(ext)) return 'image';
  if (VIDEO_EXTENSIONS.has(ext)) return 'video';
  if (AUDIO_EXTENSIONS.has(ext)) return 'audio';
  return null;
}
