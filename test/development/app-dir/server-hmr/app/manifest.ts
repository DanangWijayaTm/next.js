import type { MetadataRoute } from 'next'

export default function manifest(): MetadataRoute.Manifest {
  return {
    name: 'My App v1',
    start_url: '/',
    display: 'standalone',
  }
}
