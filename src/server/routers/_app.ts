import { z } from 'zod';
import { procedure, router } from '../trpc';

export const appRouter = router({
  hello: procedure
    .input(
      z.object({
        text: z.string(),
      }),
    )
    .query((opts) => {
      return {
        greeting: `hello ${opts.input.text}`,
      };
    }),
  getAllPosts: procedure
    .query(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").select()
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
