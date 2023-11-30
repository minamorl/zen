import * as trpcNext from '@trpc/server/adapters/next';
import { z } from 'zod'
import { procedure, router } from '../../../server/trpc'
import {createContext} from '../../../server/trpc'


// Body
export const appRouter = router({
  getPosts: procedure
    .query(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").select()
      return data 
    }),
  createPost: procedure
    .input(
      z.object({
        raw_text: z.string()
      })
    )
    .mutation(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").insert({
        raw_text: opts.input.raw_text,
        persona_id: '99c2bfd9-f5bd-47ac-ac21-b6904da987b7'
      })
      return data
     }),
  getPersonas: procedure
    .query(async (opts) => {
      const {data: user} = await opts.ctx.supabase.auth.getUser(opts.ctx.req.cookies.token)
      if (!user.user) throw new Error('User not found')
      const personas = await opts.ctx.supabase.from('personas').select().eq('user_id', user.user.id)
      return {
        user: user.user,
        personas: personas.data
      }
    }),
  signIn: procedure
    .input(
      z.object({
        email: z.string(),
        password: z.string()
      })
    )
    .mutation(async (opts) => {
      const {data: user} = await opts.ctx.supabase.auth.signInWithPassword({
        email: opts.input.email,
        password: opts.input.password
      })
      if (!user.user) {
        throw new Error()
      }
      opts.ctx.res.setHeader('set-cookie', 'token=' + user.session.access_token)
      
      const personas = await opts.ctx.supabase.from('personas').select().eq('user_id', user.user.id)
      return {
        user,
        personas
      }
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
export type AppRouter = typeof appRouter
// export API handler
// @see https://trpc.io/docs/server/adapters
export default trpcNext.createNextApiHandler({
  router: appRouter,
  createContext
});
