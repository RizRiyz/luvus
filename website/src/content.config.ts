import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  blog: defineCollection({
    loader: glob({ base: './src/content/blog', pattern: '**/*.{md,mdx}' }),
    schema: z.object({
      title: z.string(),
      description: z.string(),
      date: z.coerce.date(),
      author: z.string().default('Riz'),
      hero: z.string().optional(),
      heroAlt: z.string().optional(),
      heroWidth: z.number().int().positive().optional(),
      heroHeight: z.number().int().positive().optional(),
      draft: z.boolean().default(false),
    }),
  }),
};
