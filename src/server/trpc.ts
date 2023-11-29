import { initTRPC } from '@trpc/server';
import { CreateNextContextOptions } from '@trpc/server/adapters/next';

import { createClient } from '@supabase/supabase-js'
import { Database } from './database.types'

export const createContext = async (opts: CreateNextContextOptions) => {
  const supabaseUrl = 'https://spiwxmtkmymqltdfxogg.supabase.co'
  const supabaseKey = process.env.SUPABASE_KEY
  const supabase = createClient<Database>(supabaseUrl, supabaseKey!)
  return {
    supabase
  }
};
// Avoid exporting the entire t-object
// since it's not very descriptive.
// For instance, the use of a t variable
// is common in i18n libraries.
const t = initTRPC.context<typeof createContext>().create();
// Base router and procedure helpers
export const router = t.router;
export const procedure = t.procedure;


