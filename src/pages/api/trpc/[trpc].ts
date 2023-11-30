import * as trpcNext from '@trpc/server/adapters/next';
import { z } from 'zod'
import { procedure, router } from '../../../server/trpc'
import {createContext} from '../../../server/trpc'


// Body
export const appRouter = router({
  getPosts: procedure
    .query(async (opts) => {
      const {data} = await opts.ctx.supabase.from("posts").select(`
                                                                  id,
                                                                  raw_text,
                                                                  persona_id,
                                                                  created_at,
                                                                  threads (
                                                                    id,
                                                                    raw_text,
                                                                    persona_id,
                                                                    created_at
                                                                  )


                                                                  `).order('created_at', {
                                                                          ascending: false})
      console.log(data)
      return data 
    }),
  createPost: procedure
    .input(
      z.object({
        persona_id: z.string(),
        raw_text: z.string()
      })
    )
    .mutation(async (opts) => {
      if (!opts.ctx.user.user) throw new Error('Post creation failed')
      const personas = await opts.ctx.supabase.from("personas").select().eq('user_id', opts.ctx.user.user.id).eq('id', opts.input.persona_id)
      if (personas.count && personas.count < 1) throw new Error('Persona not found')
      const {data} = await opts.ctx.supabase.from("posts").insert({
        raw_text: opts.input.raw_text,
        persona_id: opts.input.persona_id
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
    }),
  createThread: procedure
    .input(
      z.object({
        post_id: z.string(),
        raw_text: z.string(),
        persona_id: z.string(),
      })
    )
    .mutation(async (opts) => {
      const r = await opts.ctx.supabase.from('threads').insert({
        post_id: opts.input.post_id,
        raw_text: opts.input.raw_text,
        persona_id: opts.input.persona_id
      })
      return r.data

    })
});
export type AppRouter = typeof appRouter
// export API handler
// @see https://trpc.io/docs/server/adapters
export default trpcNext.createNextApiHandler({
  router: appRouter,
  createContext
});
