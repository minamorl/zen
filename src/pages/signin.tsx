import {useState, useEffect} from 'react'

import { createClient } from '@supabase/supabase-js'
import { Auth } from '@supabase/auth-ui-react'
import { ThemeSupa } from "@supabase/auth-ui-shared";
import { Session } from 'inspector';
import { useRouter} from 'next/router'

const SUPABASE_ANON_KEY = 'eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNwaXd4bXRrbXltcWx0ZGZ4b2dnIiwicm9sZSI6ImFub24iLCJpYXQiOjE3MDEyNjk1NzMsImV4cCI6MjAxNjg0NTU3M30.aYkKWGpKvfoOj84p5T4lED0Iy6VXK8yH3Jo0h2kEQMc'
const supabase = createClient('https://spiwxmtkmymqltdfxogg.supabase.co', SUPABASE_ANON_KEY)

export default function SignInPage() {
  const router = useRouter()
  const [session, setSession] = useState<Session | null>(null)

    useEffect(() => {
      supabase.auth.getSession().then(({ data: { session } }) => {
        setSession(session)
      })

      const {
        data: { subscription },
      } = supabase.auth.onAuthStateChange((_event, session) => {
        setSession(session)
      })

      return () => subscription.unsubscribe()
    }, [])

    if (!session) {
      return (<Auth supabaseClient={supabase} appearance={{ theme: ThemeSupa }} />)
    }
    else {
      document.cookie="token="
      document.cookie = "token=" + (session as any).access_token
      router.push('/me')
      
    }
  }
