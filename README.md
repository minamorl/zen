<pre>
███████╗███████╗███╗ ██╗
╚══███╔╝██╔════╝████╗ ██║
███╔╝ ███████╗██╔██╗ ██║
███╔╝ ███════╝██║╚██╗██║
███████╗███████║██║ ╚████║
╚══════╝╚══════╝╚═╝ ╚═══╝
</pre>

# zen - A Progressive Social Web Platform

Welcome to zen, a cutting-edge social web platform designed to connect and empower communities.

## Principles

Zen is code-first. Always functioning code is correct, and everything else will follow. We prioritize release over maintenance.

The spirit of fixing things on your own is at the heart of Zen.

We leave the responsibility of maintaining data persistence not to the server side but to the users. In other words, we make no guarantees about the data.

## Getting Started

### Prerequisites

- Docker compose

### Installation

Clone the repository:

```bash

git clone https://github.com/minamorl/zen.git

```

Run the following command

```bash
docker compose up
```

You need to execute migration first. So open a new terminal and run:

```bash
docker compose run web npx prisma migrate dev
```

Then access to `localhost:3000`! Yay!

## Site

Visit [zen-kohl.vercel.app](https://zen-kohl.vercel.app/) to see the live application.

## Contributing

Contributions are what make the open-source community such an amazing place to learn, inspire, and create. Any contributions you make are greatly appreciated.

## License

Distributed under the MIT License. See LICENSE for more information.
