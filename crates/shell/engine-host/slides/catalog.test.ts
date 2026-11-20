import { describe, expect, it } from 'vitest'
import { listSlideTemplates } from '@seuoffice/pipelines/slides/templates'
import { templateCatalog } from '../slides'

describe('slide template catalog boundary', () => {
  it('returns the original catalog in display order without projecting template fields', () => {
    expect(templateCatalog()).toEqual(listSlideTemplates())
    expect(templateCatalog().length).toBeGreaterThan(0)
  })
})
