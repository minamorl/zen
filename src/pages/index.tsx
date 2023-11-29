import { useState } from 'react';
import { trpc } from '../utils/trpc';

const SignInForm = () => {
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const {data, mutate} = trpc.signIn.useMutation()
  console.log(data)

  return <div>
    <input
      type="text"
      placeholder="email"
      value={email}
      onChange={(e) => setEmail(e.target.value)}
    />
    <input
      type="text"
      placeholder="password"
      value={password}
      onChange={(e) => setPassword(e.target.value)}
    />
    <button
      onClick={() => {
        mutate({
          email,
          password
        })
      }}
    >
      Sign In
    </button>
    <h2>Persona list</h2>
    <ul>
      { Array.isArray(data) && data.map(v => <li>{v.name}</li>) }
    </ul>
    </div>
}


export default function IndexPage() {
  const {data: posts } = trpc.getAllPosts.useQuery()
  const {data } = trpc.createPost.useMutation()
  
  
  if (!posts) {
    return <div>Loading...</div>;
  }
  console.log(posts)
  return (
    <div>
      <SignInForm />

      <div>
        // create input form

        <ul>
        { posts.map(v => <li key={v.id}>{v.raw_text}</li>) }
        </ul>
      </div>
    </div>
  );
}
