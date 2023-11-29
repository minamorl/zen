import { z } from 'zod';
import { procedure, router } from '../trpc';

export const appRouter = router({
  getAllPosts: procedure
    .query(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").select()
      return data 
    }),
  createPost: procedure
    .query(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").insert({
        raw_text: 'testtest',
        persona_id: '99c2bfd9-f5bd-47ac-ac21-b6904da987b7'
      })
      return data
     }),
  createUser: procedure
    .input(
      z.object({
        email: z.string(),
        password: z.string()
      })
    )
    .query(async (opts) => {
      const {data, error} = await opts.ctx.supabase.auth.signUp({
        email: opts.input.email,
        password: opts.input.password
      })
      if (error) return error
      return data
    })
});
// export type definition of API
export type AppRouter = typeof appRouter;
